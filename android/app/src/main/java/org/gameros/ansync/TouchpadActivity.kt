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
import androidx.compose.ui.layout.onGloballyPositioned
import androidx.compose.ui.layout.onSizeChanged
import androidx.compose.ui.layout.positionInWindow
import androidx.compose.ui.platform.LocalContext
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

    /// Canvas rect in window coords. Set from the composable via
    /// `onGloballyPositioned`; read from `dispatchGenericMotionEvent`
    /// to translate stylus-hover events (which arrive at the activity
    /// before the View tree, *not* through `pointerInteropFilter`)
    /// back into canvas-local coords.
    @Volatile var canvasLeft: Float = 0f
    @Volatile var canvasTop: Float = 0f
    @Volatile var canvasWidth: Int = 0
    @Volatile var canvasHeight: Int = 0

    /// Palm rejection state. While `penActive == true` (pen in
    /// contact OR in proximity) every finger pointer in every
    /// MotionEvent is dropped — only the pen pointer in the event
    /// is forwarded to the host. After the pen leaves proximity we
    /// also latch finger rejection for [PEN_LIFT_LATCH_MS] so the
    /// palm settling on the screen as the user pulls the pen away
    /// doesn't immediately fire a cursor jump or click.
    @Volatile var penActive: Boolean = false
    @Volatile var penReleasedAt: Long = 0L

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

    /**
     * Stylus *hover* events (`ACTION_HOVER_*`) are delivered through
     * `dispatchGenericMotionEvent`, not the touch dispatch path that
     * `pointerInteropFilter` taps into — so they would otherwise be
     * silently dropped. Forward them to the same `handleStylus`
     * pipeline so the host's uinput pen tracks pen position even
     * before the tip touches the screen (matches Wacom / Surface UX:
     * cursor follows pen mid-air, click happens on contact).
     */
    override fun dispatchGenericMotionEvent(event: MotionEvent): Boolean {
        val tool = if (event.pointerCount > 0) event.getToolType(0) else MotionEvent.TOOL_TYPE_UNKNOWN
        val isPen = tool == MotionEvent.TOOL_TYPE_STYLUS || tool == MotionEvent.TOOL_TYPE_ERASER
        if (!isPen) return super.dispatchGenericMotionEvent(event)
        if (canvasWidth <= 0 || canvasHeight <= 0) return super.dispatchGenericMotionEvent(event)
        when (event.actionMasked) {
            MotionEvent.ACTION_HOVER_ENTER,
            MotionEvent.ACTION_HOVER_MOVE,
            MotionEvent.ACTION_HOVER_EXIT,
            -> {
                val localX = event.x - canvasLeft
                val localY = event.y - canvasTop
                if (localX < 0f || localY < 0f ||
                    localX > canvasWidth || localY > canvasHeight
                ) {
                    // Outside the canvas (toolbar area, etc.) — let
                    // the system handle it normally.
                    return super.dispatchGenericMotionEvent(event)
                }
                val proximityOut = event.actionMasked == MotionEvent.ACTION_HOVER_EXIT
                if (proximityOut) {
                    penActive = false
                    penReleasedAt = SystemClock.uptimeMillis()
                } else {
                    penActive = true
                }
                emitStylusFromHover(
                    event = event,
                    localX = localX,
                    localY = localY,
                    canvasW = canvasWidth,
                    canvasH = canvasHeight,
                    proximityOut = proximityOut,
                    eraser = tool == MotionEvent.TOOL_TYPE_ERASER,
                )
                return true
            }
        }
        return super.dispatchGenericMotionEvent(event)
    }
}

/// Window after the pen leaves the surface during which finger /
/// palm touches are still ignored. Matches the timing scrcpy / Samsung
/// Notes use for the same effect.
private const val PEN_LIFT_LATCH_MS = 250L
private const val LONG_PRESS_MS = 450L
private const val TAP_SLOP_PX = 12f
private const val TAP_MAX_MS = 200L
private const val DOUBLE_TAP_MS = 300L
private const val STYLUS_ABS_MAX = 32767
private const val STYLUS_PRESSURE_MAX = 8191
/// Hi-res wheel ticks emitted per pixel of finger travel. 120 ticks
/// equal one legacy notch, so this factor lands ~3-4 notches per 100
/// px of swipe — close to how a physical trackpad behaves on the
/// same hardware.
private const val WHEEL_HI_RES_PER_PIXEL = 4f
/// Pixels of dominant axis travel before a 2-finger gesture commits
/// to scroll-vs-pinch mode. Below this both are still being
/// measured; whichever crossed first wins for the rest of the
/// gesture.
private const val MODE_LOCK_PX = 16f
/// Hi-res wheel ticks emitted per pixel of pinch spread / contract.
/// Pinch mode wraps the wheel in Ctrl press/release so apps see the
/// universal `Ctrl+Scroll = zoom` shortcut.
private const val PINCH_HI_RES_PER_PIXEL = 3f
private const val TWO_FINGER_MODE_UNDECIDED = 0
private const val TWO_FINGER_MODE_SCROLL = 1
private const val TWO_FINGER_MODE_PINCH = 2
/// Linux evdev `KEY_LEFTCTRL`.
private const val EVDEV_LEFTCTRL = 29

@OptIn(ExperimentalComposeUiApi::class)
@Composable
private fun TouchpadScreen() {
    var status by remember { mutableStateOf("touchpad ready") }
    var canvasSize by remember { mutableStateOf(IntSize.Zero) }
    var imeOpen by remember { mutableStateOf(false) }
    var editTextRef by remember { mutableStateOf<HostKeyboardEditText?>(null) }
    val activity = LocalContext.current as? TouchpadActivity
    /// When `true`, every pointer in every `MotionEvent` is
    /// forwarded straight to the host's uinput Touchscreen (MT-B)
    /// device. The Android view becomes a 1:1 absolute-coord touch
    /// overlay of the host display: pinch / pan / rotate are all
    /// resolved by the host's compositor and apps natively, which
    /// gives precise simultaneous control instead of the synthesised
    /// `Ctrl+Wheel` zoom of trackpad mode.
    var rawTouchMode by remember { mutableStateOf(false) }

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
            Button(onClick = { rawTouchMode = !rawTouchMode }) {
                Text(if (rawTouchMode) "Trackpad mode" else "Raw touch mode")
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
                .onSizeChanged {
                    canvasSize = it
                    activity?.canvasWidth = it.width
                    activity?.canvasHeight = it.height
                }
                .onGloballyPositioned { coords ->
                    val pos = coords.positionInWindow()
                    activity?.canvasLeft = pos.x
                    activity?.canvasTop = pos.y
                }
                .pointerInteropFilter { event ->
                    if (rawTouchMode) {
                        handleRawTouchEvent(activity, event, canvasSize)
                        status = "raw touch — ${event.pointerCount} fingers"
                    } else {
                        val update = handlePointerEvent(activity, event, canvasSize)
                        if (update != null) status = update
                    }
                    true
                },
        ) {
            val helpText = if (rawTouchMode) {
                "raw touch overlay → host MT-B Touchscreen\n" +
                    "every finger forwarded with absolute coords\n" +
                    "pinch / pan / rotate handled by the host compositor\n" +
                    "no synthesised clicks — apps see real touch events"
            } else {
                "drag → cursor  •  tap → click  •  long press → right\n" +
                    "double-tap + hold → left button drag\n" +
                    "2-finger drag → wheel  •  2-finger tap → middle\n" +
                    "pinch fingers → Ctrl+Wheel zoom\n" +
                    "stylus → pen events  •  Show keyboard → type to host"
            }
            Text(
                text = helpText,
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
    var twoFingerStartX: Float = 0f,
    var twoFingerStartY: Float = 0f,
    var twoFingerMoved: Boolean = false,
    var lastUpAt: Long = 0L,
    var wheelRemainderX: Float = 0f,
    var wheelRemainderY: Float = 0f,
    /// 0 = undecided, 1 = scroll, 2 = pinch. Locked at the moment
    /// either accumulator crosses `MODE_LOCK_PX`.
    var twoFingerMode: Int = TWO_FINGER_MODE_UNDECIDED,
    /// Sum of |center axis delta| since gesture start. Used to
    /// classify scroll vs pinch during the undecided window.
    var scrollAccum: Float = 0f,
    /// Sum of |distance delta between the two pointers| since
    /// gesture start.
    var pinchAccum: Float = 0f,
    /// Distance between the two pointers at the previous MOVE event.
    var pinchLastDistance: Float = 0f,
    /// Sub-tick carry for pinch → hi-res wheel conversion.
    var pinchRemainder: Float = 0f,
    /// Whether `KEY_LEFTCTRL` is currently held on the host because
    /// the gesture is in pinch mode (released at UP / CANCEL /
    /// POINTER_DOWN→scroll downgrade).
    var ctrlHeld: Boolean = false,
)

private val gesture = Gesture()

private fun handlePointerEvent(
    activity: TouchpadActivity?,
    event: MotionEvent,
    canvas: IntSize,
): String? {
    // Palm rejection: if the pen is present in this event, the only
    // pointer we ever forward is the pen itself. Any finger / palm
    // pointers in the same MotionEvent are dropped wholesale.
    val penIdx = scanPenIndex(event)
    if (penIdx >= 0) {
        // POINTER_DOWN / POINTER_UP whose actionIndex is a finger
        // means a palm just landed or lifted while the pen is in
        // contact — there is no pen state change to report, so we
        // ignore the event entirely (handleStylus would re-emit the
        // pen position needlessly).
        val act = event.actionMasked
        if ((act == MotionEvent.ACTION_POINTER_DOWN || act == MotionEvent.ACTION_POINTER_UP) &&
            event.actionIndex != penIdx
        ) {
            return "palm rejected (pen down)"
        }
        val penTool = event.getToolType(penIdx)
        return handleStylus(
            activity = activity,
            event = event,
            canvas = canvas,
            idx = penIdx,
            eraser = penTool == MotionEvent.TOOL_TYPE_ERASER,
        )
    }

    // No pen in this event. Drop finger touches while the pen is
    // still in proximity OR within the post-lift latch window — that
    // is the palm settling on the screen as the user pulls the pen
    // away, and acting on it would jump the cursor / fire spurious
    // clicks.
    if (activity != null && isPenLatchActive(activity)) {
        // Cancel any in-flight gesture so a finger that started
        // before the pen entered proximity doesn't continue.
        if (gesture.leftHeld) { sendButton(button = 1, pressed = false); gesture.leftHeld = false }
        if (gesture.rightHeld) { sendButton(button = 2, pressed = false); gesture.rightHeld = false }
        releaseCtrlIfHeld()
        gesture.twoFingerActive = false
        return "palm rejected (pen latch)"
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
            // two-finger scroll / pinch / middle-click stream. Any
            // drag-mode single-finger gesture in progress is
            // cancelled (button release) before flipping.
            if (gesture.leftHeld) {
                sendButton(button = 1, pressed = false)
                gesture.leftHeld = false
            }
            gesture.twoFingerActive = true
            val cx = if (event.pointerCount >= 2) (event.getX(0) + event.getX(1)) / 2 else event.x
            val cy = if (event.pointerCount >= 2) (event.getY(0) + event.getY(1)) / 2 else event.y
            gesture.twoFingerLastX = cx
            gesture.twoFingerLastY = cy
            gesture.twoFingerStartX = cx
            gesture.twoFingerStartY = cy
            gesture.twoFingerMoved = false
            gesture.wheelRemainderX = 0f
            gesture.wheelRemainderY = 0f
            gesture.twoFingerMode = TWO_FINGER_MODE_UNDECIDED
            gesture.scrollAccum = 0f
            gesture.pinchAccum = 0f
            gesture.pinchRemainder = 0f
            gesture.pinchLastDistance = if (event.pointerCount >= 2) {
                kotlin.math.hypot(
                    (event.getX(0) - event.getX(1)).toDouble(),
                    (event.getY(0) - event.getY(1)).toDouble(),
                ).toFloat()
            } else 0f
            "two-finger active"
        }
        MotionEvent.ACTION_MOVE -> {
            if (gesture.twoFingerActive) {
                handleTwoFingerMove(event)
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
        MotionEvent.ACTION_POINTER_UP -> {
            // First of the two fingers lifted. Release Ctrl now so
            // a stuck modifier never escapes the gesture, even if
            // the user keeps the remaining finger down without ever
            // hitting ACTION_UP.
            releaseCtrlIfHeld()
            "two-finger ending"
        }
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
            releaseCtrlIfHeld()
            gesture.leftHeld = false
            gesture.rightHeld = false
            gesture.twoFingerActive = false
            "pointer-up"
        }
        MotionEvent.ACTION_CANCEL -> {
            if (gesture.leftHeld) sendButton(button = 1, pressed = false)
            if (gesture.rightHeld) sendButton(button = 2, pressed = false)
            releaseCtrlIfHeld()
            gesture.leftHeld = false
            gesture.rightHeld = false
            gesture.twoFingerActive = false
            "cancelled"
        }
        else -> null
    }
}

/**
 * Two-finger ACTION_MOVE branch. Resolves the gesture into either
 * a scroll (emit `MouseWheel` from the centroid delta) or a pinch
 * (emit `Ctrl+MouseWheel` from the inter-finger distance delta).
 * The decision is locked the first time either accumulator crosses
 * [MODE_LOCK_PX] so the mid-gesture intent is stable.
 */
private fun handleTwoFingerMove(event: MotionEvent): String {
    if (event.pointerCount < 2) return "wheel"
    val p0x = event.getX(0); val p0y = event.getY(0)
    val p1x = event.getX(1); val p1y = event.getY(1)
    val cx = (p0x + p1x) / 2; val cy = (p0y + p1y) / 2
    val distance = kotlin.math.hypot((p0x - p1x).toDouble(), (p0y - p1y).toDouble()).toFloat()

    val centerDx = cx - gesture.twoFingerLastX
    val centerDy = cy - gesture.twoFingerLastY
    val distanceDelta = distance - gesture.pinchLastDistance

    // Update mode-classification accumulators while still undecided.
    if (gesture.twoFingerMode == TWO_FINGER_MODE_UNDECIDED) {
        gesture.scrollAccum += kotlin.math.abs(centerDy)
        gesture.pinchAccum += kotlin.math.abs(distanceDelta)
        val travelled = kotlin.math.hypot(
            (cx - gesture.twoFingerStartX).toDouble(),
            (cy - gesture.twoFingerStartY).toDouble(),
        )
        if (travelled > TAP_SLOP_PX || gesture.pinchAccum > TAP_SLOP_PX) {
            gesture.twoFingerMoved = true
        }
        if (gesture.scrollAccum >= MODE_LOCK_PX || gesture.pinchAccum >= MODE_LOCK_PX) {
            gesture.twoFingerMode = if (gesture.pinchAccum > gesture.scrollAccum) {
                TWO_FINGER_MODE_PINCH
            } else {
                TWO_FINGER_MODE_SCROLL
            }
            if (gesture.twoFingerMode == TWO_FINGER_MODE_PINCH && !gesture.ctrlHeld) {
                sendKey(EVDEV_LEFTCTRL, true)
                gesture.ctrlHeld = true
            }
        }
    }

    when (gesture.twoFingerMode) {
        TWO_FINGER_MODE_SCROLL, TWO_FINGER_MODE_UNDECIDED -> {
            // Pure scroll path — same smooth hi-res wheel emission
            // as the previous single-mode implementation.
            gesture.wheelRemainderX += centerDx * WHEEL_HI_RES_PER_PIXEL
            // Y-up == wheel-up == positive `REL_WHEEL`.
            gesture.wheelRemainderY += -centerDy * WHEEL_HI_RES_PER_PIXEL
            val wheelX = gesture.wheelRemainderX.toInt()
            val wheelY = gesture.wheelRemainderY.toInt()
            if (wheelX != 0 || wheelY != 0) {
                gesture.wheelRemainderX -= wheelX.toFloat()
                gesture.wheelRemainderY -= wheelY.toFloat()
                NativeBridge.nativeSendInputMessage(
                    WireInputMessage.MouseWheel(dx = wheelX, dy = wheelY).encode()
                )
            }
        }
        TWO_FINGER_MODE_PINCH -> {
            // Pinch path — Ctrl is already held; positive distance
            // delta (spread) maps to wheel-up (zoom in).
            gesture.pinchRemainder += distanceDelta * PINCH_HI_RES_PER_PIXEL
            val zoom = gesture.pinchRemainder.toInt()
            if (zoom != 0) {
                gesture.pinchRemainder -= zoom.toFloat()
                NativeBridge.nativeSendInputMessage(
                    WireInputMessage.MouseWheel(dx = 0, dy = zoom).encode()
                )
            }
        }
    }

    gesture.twoFingerLastX = cx
    gesture.twoFingerLastY = cy
    gesture.pinchLastDistance = distance
    return when (gesture.twoFingerMode) {
        TWO_FINGER_MODE_PINCH -> "pinch"
        TWO_FINGER_MODE_SCROLL -> "wheel"
        else -> "two-finger"
    }
}

private fun releaseCtrlIfHeld() {
    if (gesture.ctrlHeld) {
        sendKey(EVDEV_LEFTCTRL, false)
        gesture.ctrlHeld = false
    }
}

// ── Raw touch (MT-B passthrough) ─────────────────────────────────────

/**
 * Forward every pointer in [event] straight to the host's uinput
 * Touchscreen device as `TouchSlot` packets. Slot + tracking id come
 * from Android's stable `pointerId` (Linux MT-B expects tracking id
 * >= 0 while contact, -1 on release). Coords map the local Compose
 * canvas to the host display's 0..32767 ABS range linearly.
 *
 * Mode is exclusive — when the toggle is on, none of the
 * trackpad-style synthesis (mouse buttons, wheel, pinch→Ctrl+Wheel)
 * fires. The host sees a true multi-touch stream and resolves
 * gestures via the focused compositor / app, which is what apps
 * with native touch handling (browsers, GIMP/Krita with touch,
 * Wayland compositors with libinput gestures) actually want.
 */
private fun handleRawTouchEvent(activity: TouchpadActivity?, event: MotionEvent, canvas: IntSize) {
    if (canvas.width <= 0 || canvas.height <= 0) return
    // In raw-touch mode the stylus still bypasses the touchscreen
    // forwarder and goes through the dedicated uinput pen device —
    // route the pen pointer (if any) to handleStylus and continue
    // emitting only the *non-finger* pointers below.
    val penIdx = scanPenIndex(event)
    if (penIdx >= 0) {
        val penTool = event.getToolType(penIdx)
        handleStylus(
            activity = activity,
            event = event,
            canvas = canvas,
            idx = penIdx,
            eraser = penTool == MotionEvent.TOOL_TYPE_ERASER,
        )
    }
    val latched = activity != null && isPenLatchActive(activity)
    when (event.actionMasked) {
        MotionEvent.ACTION_DOWN, MotionEvent.ACTION_POINTER_DOWN -> {
            if (event.actionIndex == penIdx) return
            if (latched && event.getToolType(event.actionIndex) == MotionEvent.TOOL_TYPE_FINGER) return
            emitTouchSlot(event, event.actionIndex, canvas, lifted = false)
        }
        MotionEvent.ACTION_MOVE -> {
            for (i in 0 until event.pointerCount) {
                if (i == penIdx) continue
                if (latched && event.getToolType(i) == MotionEvent.TOOL_TYPE_FINGER) continue
                emitTouchSlot(event, i, canvas, lifted = false)
            }
        }
        MotionEvent.ACTION_UP, MotionEvent.ACTION_POINTER_UP -> {
            if (event.actionIndex == penIdx) return
            emitTouchSlot(event, event.actionIndex, canvas, lifted = true)
        }
        MotionEvent.ACTION_CANCEL -> {
            for (i in 0 until event.pointerCount) {
                if (i == penIdx) continue
                emitTouchSlot(event, i, canvas, lifted = true)
            }
        }
    }
}

/// Return the index of the first stylus / eraser pointer in [event],
/// or -1 if there are none. Used by every input path to peel the pen
/// off and treat it independently from finger / palm pointers.
private fun scanPenIndex(event: MotionEvent): Int {
    for (i in 0 until event.pointerCount) {
        val t = event.getToolType(i)
        if (t == MotionEvent.TOOL_TYPE_STYLUS || t == MotionEvent.TOOL_TYPE_ERASER) {
            return i
        }
    }
    return -1
}

/// True while finger pointers should be rejected — i.e. the pen is
/// currently in proximity, or the post-lift latch window has not
/// elapsed yet.
private fun isPenLatchActive(activity: TouchpadActivity): Boolean {
    if (activity.penActive) return true
    val released = activity.penReleasedAt
    if (released == 0L) return false
    return (SystemClock.uptimeMillis() - released) < PEN_LIFT_LATCH_MS
}

private fun emitTouchSlot(event: MotionEvent, idx: Int, canvas: IntSize, lifted: Boolean) {
    val pointerId = event.getPointerId(idx)
    val slot = (pointerId and 0xFF).toByte()
    val trackingId = if (lifted) -1 else pointerId
    val absX = (event.getX(idx).coerceIn(0f, canvas.width.toFloat()) *
        STYLUS_ABS_MAX / canvas.width).toInt().coerceIn(0, STYLUS_ABS_MAX)
    val absY = (event.getY(idx).coerceIn(0f, canvas.height.toFloat()) *
        STYLUS_ABS_MAX / canvas.height).toInt().coerceIn(0, STYLUS_ABS_MAX)
    val pressure = (event.getPressure(idx).coerceIn(0f, 1f) * 255).toInt()
        .coerceIn(0, 255)
    NativeBridge.nativeSendInputMessage(
        WireInputMessage.TouchSlot(
            slot = slot,
            x = absX,
            y = absY,
            pressure = pressure,
            trackingId = trackingId,
        ).encode()
    )
}

/**
 * Touch-path stylus handler. Builds the wire packet from a
 * MotionEvent that came through `dispatchTouchEvent` (i.e. the pen
 * is already in contact). Proximity is always on for these events;
 * hover events take the `dispatchGenericMotionEvent` path via
 * [emitStylusFromHover].
 *
 * `btn` byte semantics (mirror of the Rust uinput consumer):
 *   bit 0 : BARREL primary button held
 *   bit 1 : BARREL secondary button held
 *   bit 2 : eraser tool active (host emits BTN_TOOL_RUBBER instead of BTN_TOOL_PEN)
 *   bit 7 : in-proximity (cleared on ACTION_HOVER_EXIT)
 */
private fun handleStylus(
    activity: TouchpadActivity?,
    event: MotionEvent,
    canvas: IntSize,
    idx: Int,
    eraser: Boolean,
): String {
    val rawX = event.getX(idx); val rawY = event.getY(idx)
    val absX = if (canvas.width > 0) {
        (rawX.coerceIn(0f, canvas.width.toFloat()) * STYLUS_ABS_MAX / canvas.width).toInt()
    } else 0
    val absY = if (canvas.height > 0) {
        (rawY.coerceIn(0f, canvas.height.toFloat()) * STYLUS_ABS_MAX / canvas.height).toInt()
    } else 0
    // Pen lifts from the surface are signalled either by a pressure
    // reading of 0 (preferred — the OEM driver reports it directly)
    // or by ACTION_UP / ACTION_POINTER_UP whose `actionIndex` is the
    // pen pointer. Read pressure directly off the pen slot — it's
    // already 0 in both lift cases on every device we've tested.
    val act = event.actionMasked
    val penLifting = (act == MotionEvent.ACTION_UP) ||
        (act == MotionEvent.ACTION_CANCEL) ||
        (act == MotionEvent.ACTION_POINTER_UP && event.actionIndex == idx)
    val pressure = if (penLifting) 0 else {
        (event.getPressure(idx).coerceIn(0f, 1f) * STYLUS_PRESSURE_MAX).toInt()
            .coerceIn(0, STYLUS_PRESSURE_MAX)
    }
    val (tiltX, tiltY) = computeTilt(event, idx)
    val btnState = event.buttonState
    var btn = 0x80   // in-proximity (touch path always has the pen near the surface)
    if (eraser) btn = btn or 0x4
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
    if (activity != null) {
        if (penLifting) {
            // Stamp release time so finger touches stay blocked for
            // PEN_LIFT_LATCH_MS even on devices that don't emit
            // ACTION_HOVER_EXIT after the lift. If hover events do
            // fire, the proximity-out branch will refresh the stamp.
            activity.penActive = false
            activity.penReleasedAt = SystemClock.uptimeMillis()
        } else {
            activity.penActive = true
        }
    }
    return if (eraser) "eraser p=$pressure" else "stylus p=$pressure"
}

/**
 * Hover-path stylus handler. Called from the activity-level
 * `dispatchGenericMotionEvent` override for `ACTION_HOVER_*` events,
 * which `pointerInteropFilter` never sees. Coords arrive in window
 * space; the caller has already subtracted the canvas top-left.
 *
 * Pressure is always 0 here (pen is in proximity but not touching);
 * the host's uinput consumer treats this as BTN_TOOL_PEN=1 +
 * BTN_TOUCH=0 — cursor follows the pen without clicking.
 */
internal fun emitStylusFromHover(
    event: MotionEvent,
    localX: Float,
    localY: Float,
    canvasW: Int,
    canvasH: Int,
    proximityOut: Boolean,
    eraser: Boolean,
) {
    if (canvasW <= 0 || canvasH <= 0) return
    val absX = (localX.coerceIn(0f, canvasW.toFloat()) * STYLUS_ABS_MAX / canvasW).toInt()
    val absY = (localY.coerceIn(0f, canvasH.toFloat()) * STYLUS_ABS_MAX / canvasH).toInt()
    val penIdx = scanPenIndex(event).coerceAtLeast(0)
    val (tiltX, tiltY) = computeTilt(event, penIdx)
    val btnState = event.buttonState
    var btn = 0
    if (!proximityOut) btn = btn or 0x80
    if (eraser) btn = btn or 0x4
    if ((btnState and MotionEvent.BUTTON_STYLUS_PRIMARY) != 0) btn = btn or 0x1
    if ((btnState and MotionEvent.BUTTON_STYLUS_SECONDARY) != 0) btn = btn or 0x2
    NativeBridge.nativeSendInputMessage(
        WireInputMessage.Stylus(
            x = absX,
            y = absY,
            pressure = 0,
            tiltX = tiltX,
            tiltY = tiltY,
            btn = btn.toByte(),
        ).encode()
    )
}

private fun computeTilt(event: MotionEvent, idx: Int = 0): Pair<Short, Short> {
    val tilt = event.getAxisValue(MotionEvent.AXIS_TILT, idx)
    val orient = event.getOrientation(idx)
    val degs = (tilt * 180.0 / Math.PI).toFloat()
    val tiltX = (degs * kotlin.math.cos(orient.toDouble())).toInt()
        .coerceIn(-90, 90).toShort()
    val tiltY = (degs * kotlin.math.sin(orient.toDouble())).toInt()
        .coerceIn(-90, 90).toShort()
    return tiltX to tiltY
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
