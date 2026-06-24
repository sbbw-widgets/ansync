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
 * peer.
 *
 *  ┌─ Touch (default: touchpad mode) ────────────────────────────────┐
 *  │ Every finger pointer becomes a raw `TouchpadSlot` (MT-B) packet │
 *  │ aimed at the host's clickpad uinput device. libinput on the     │
 *  │ host drives ALL gesture detection: tap-to-click, two-finger     │
 *  │ scroll, pinch zoom, drag-lock, palm rejection — configured      │
 *  │ from the user's compositor input settings. No per-finger        │
 *  │ synthesis lives on the companion any more.                      │
 *  │                                                                 │
 *  │ The "Touchscreen mode" toggle flips to `TouchSlot` packets so   │
 *  │ the same fingers land on the host's absolute-coord touchscreen  │
 *  │ uinput device — useful for apps that want raw multi-touch       │
 *  │ instead of pointer gestures.                                    │
 *  └────────────────────────────────────────────────────────────────┘
 *
 *  ┌─ Stylus ───────────────────────────────────────────────────────┐
 *  │ Pen / eraser pointers always take the dedicated tablet path    │
 *  │ regardless of mode → `Stylus { x, y, pressure, tilt, btn }` →  │
 *  │ host uinput Stylus (Wacom-style indirect tablet). Hover events │
 *  │ ride through `dispatchGenericMotionEvent` so the cursor tracks │
 *  │ the pen mid-air.                                               │
 *  └────────────────────────────────────────────────────────────────┘
 *
 *  ┌─ Keyboard ─────────────────────────────────────────────────────┐
 *  │ Hardware KeyEvent → `KeyPress` via `dispatchKeyEvent`. Soft IME│
 *  │ → offscreen `EditText` whose `InputConnection` intercepts      │
 *  │ `commitText` / `deleteSurroundingText` / `sendKeyEvent` 1-to-1.│
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
private const val STYLUS_ABS_MAX = 32767
private const val STYLUS_PRESSURE_MAX = 8191
/// Absolute coord upper bound for touchpad / touchscreen slots —
/// matches the `ABS_MAX` advertised on the host uinput devices.
private const val TOUCH_ABS_MAX = 32767

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
                Text(if (rawTouchMode) "Touchpad mode" else "Touchscreen mode")
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
                    // Stylus pointers always take the dedicated tablet
                    // pipeline regardless of mode.
                    val pen = scanPenIndex(event)
                    if (pen >= 0) {
                        val update = handlePointerEvent(activity, event, canvasSize)
                        if (update != null) status = update
                    } else if (rawTouchMode) {
                        handleRawTouchEvent(activity, event, canvasSize)
                        status = "raw touch — ${event.pointerCount} fingers"
                    } else {
                        handleTouchpadEvent(activity, event, canvasSize)
                        status = "touchpad — ${event.pointerCount} fingers"
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
                "Mac-style touchpad → host clickpad\n" +
                    "tap-to-click, two-finger scroll, pinch zoom handled\n" +
                    "by libinput on the host (configure in your compositor)\n" +
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

// ── Touch / stylus dispatch ──────────────────────────────────────────

/**
 * Stylus-only path. Called from the touch dispatcher when a stylus
 * pointer is present in the MotionEvent; the finger pointers (palm)
 * in the same event are dropped wholesale so the host pen doesn't
 * see them. Returns `null` when there is no stylus pointer at all —
 * the caller should fall through to [handleTouchpadEvent].
 */
private fun handlePointerEvent(
    activity: TouchpadActivity?,
    event: MotionEvent,
    canvas: IntSize,
): String? {
    val penIdx = scanPenIndex(event)
    if (penIdx < 0) return null
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

/**
 * Touchpad path. Forwards every finger pointer in [event] straight
 * to the host's uinput *touchpad* (clickpad) device as raw MT-B
 * `TouchpadSlot` packets. libinput on the host then drives every
 * gesture (tap-to-click, two-finger scroll, pinch zoom, palm
 * rejection) via the compositor's input config — there is no
 * per-finger gesture synthesis on the companion side.
 *
 * The companion still owns the pen-latch palm rejection because
 * the stylus is a separate uinput device (libinput can't correlate
 * a stylus on one node with a finger on another) — every finger
 * touch is dropped while the pen is in proximity OR within the
 * post-lift window.
 */
private fun handleTouchpadEvent(
    activity: TouchpadActivity?,
    event: MotionEvent,
    canvas: IntSize,
) {
    if (canvas.width <= 0 || canvas.height <= 0) return
    if (activity != null && isPenLatchActive(activity)) return
    when (event.actionMasked) {
        MotionEvent.ACTION_DOWN, MotionEvent.ACTION_POINTER_DOWN -> {
            emitTouchpadSlot(event, event.actionIndex, canvas, lifted = false)
        }
        MotionEvent.ACTION_MOVE -> {
            // Android batches sub-frame motion into the MotionEvent's
            // historical samples (`event.historySize` of them, each
            // ~8 ms apart). Emit them in order BEFORE the current
            // sample so libinput's per-event delta stays small —
            // otherwise a single ACTION_MOVE that covers 3-5 sub-
            // frames in one shot looks like a 30 mm finger
            // teleport and gets discarded as a "Touch jump".
            val historySize = event.historySize
            for (h in 0 until historySize) {
                for (i in 0 until event.pointerCount) {
                    emitTouchpadSlotHistorical(event, i, h, canvas)
                }
            }
            for (i in 0 until event.pointerCount) {
                emitTouchpadSlot(event, i, canvas, lifted = false)
            }
        }
        MotionEvent.ACTION_UP, MotionEvent.ACTION_POINTER_UP -> {
            emitTouchpadSlot(event, event.actionIndex, canvas, lifted = true)
        }
        MotionEvent.ACTION_CANCEL -> {
            for (i in 0 until event.pointerCount) {
                emitTouchpadSlot(event, i, canvas, lifted = true)
            }
        }
    }
}

private fun emitTouchpadSlotHistorical(
    event: MotionEvent,
    idx: Int,
    historyIdx: Int,
    canvas: IntSize,
) {
    val pointerId = event.getPointerId(idx)
    val slot = (pointerId and 0xFF).toByte()
    val trackingId = activeTouchpadTracking[pointerId] ?: return
    val absX = (event.getHistoricalX(idx, historyIdx).coerceIn(0f, canvas.width.toFloat()) *
        TOUCH_ABS_MAX / canvas.width).toInt().coerceIn(0, TOUCH_ABS_MAX)
    val absY = (event.getHistoricalY(idx, historyIdx).coerceIn(0f, canvas.height.toFloat()) *
        TOUCH_ABS_MAX / canvas.height).toInt().coerceIn(0, TOUCH_ABS_MAX)
    val pressure = (event.getHistoricalPressure(idx, historyIdx).coerceIn(0f, 1f) * 255).toInt()
        .coerceIn(0, 255)
    NativeBridge.nativeSendInputMessage(
        WireInputMessage.TouchpadSlot(
            slot = slot,
            x = absX,
            y = absY,
            pressure = pressure,
            trackingId = trackingId,
        ).encode()
    )
}

/// Maps Android's reusable `pointerId` to a monotonic tracking_id so
/// libinput's MT-B "Touch jump" heuristic doesn't kick in when two
/// consecutive single-finger taps both come in on Android's pointer
/// id 0. Each new touchdown allocates a fresh id; lift removes the
/// entry. The counter wraps at 0xFFFF (the kernel max we advertise).
private val activeTouchpadTracking = HashMap<Int, Int>()
private var nextTouchpadTrackingId = 0

private fun emitTouchpadSlot(event: MotionEvent, idx: Int, canvas: IntSize, lifted: Boolean) {
    val pointerId = event.getPointerId(idx)
    val slot = (pointerId and 0xFF).toByte()
    val trackingId: Int = if (lifted) {
        activeTouchpadTracking.remove(pointerId)
        -1
    } else {
        activeTouchpadTracking.getOrPut(pointerId) {
            val tid = nextTouchpadTrackingId
            nextTouchpadTrackingId = (nextTouchpadTrackingId + 1) and 0xFFFF
            tid
        }
    }
    val absX = (event.getX(idx).coerceIn(0f, canvas.width.toFloat()) *
        TOUCH_ABS_MAX / canvas.width).toInt().coerceIn(0, TOUCH_ABS_MAX)
    val absY = (event.getY(idx).coerceIn(0f, canvas.height.toFloat()) *
        TOUCH_ABS_MAX / canvas.height).toInt().coerceIn(0, TOUCH_ABS_MAX)
    val pressure = (event.getPressure(idx).coerceIn(0f, 1f) * 255).toInt()
        .coerceIn(0, 255)
    NativeBridge.nativeSendInputMessage(
        WireInputMessage.TouchpadSlot(
            slot = slot,
            x = absX,
            y = absY,
            pressure = pressure,
            trackingId = trackingId,
        ).encode()
    )
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
