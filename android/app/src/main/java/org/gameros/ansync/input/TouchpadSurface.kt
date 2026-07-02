package org.gameros.ansync.input

import android.os.SystemClock
import android.view.MotionEvent
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
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
import androidx.compose.ui.unit.IntSize
import androidx.compose.ui.unit.dp
import com.composables.icons.lucide.Info
import com.composables.icons.lucide.Lucide
import org.gameros.ansync.NativeBridge
import org.gameros.ansync.WireInputMessage

/**
 * Touchpad + stylus surface. Every finger becomes a raw
 * `TouchpadSlot` (MT-B) packet aimed at the host's clickpad uinput
 * device. libinput on the host drives ALL gesture detection:
 * tap-to-click, two-finger scroll, pinch zoom, drag-lock, palm
 * rejection — configured from the user's compositor input settings.
 *
 * The status pill in the top-right doubles as a mode switch:
 * flipping to "Touchscreen mode" routes packets to the host's
 * uinput touchscreen instead — useful for apps that want raw
 * multi-touch instead of pointer gestures. Stylus pointers always
 * bypass both modes and take the dedicated pen device path.
 *
 * A physical keyboard attached to the phone forwards through
 * [InputActivity.dispatchKeyEvent] regardless of which input mode
 * the rail has selected.
 */
@OptIn(ExperimentalComposeUiApi::class)
@Composable
fun TouchpadSurface() {
    var status by remember { mutableStateOf("touchpad ready") }
    var canvasSize by remember { mutableStateOf(IntSize.Zero) }
    val activity = LocalContext.current as? InputActivity
    /// When `true`, every pointer in every `MotionEvent` is forwarded
    /// straight to the host's uinput Touchscreen (MT-B) device.
    var rawTouchMode by remember { mutableStateOf(false) }

    // Outer box carries no pointer input — canvas + overlays are its
    // siblings so overlay clicks (Switch, KeyboardStatusPill) reach
    // their own pointer handlers instead of being swallowed by the
    // canvas's `pointerInteropFilter`.
    Box(
        modifier = Modifier
            .fillMaxSize()
            .background(Color(0xFF101418)),
    ) {
        Box(
            modifier = Modifier
                .fillMaxSize()
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
        )
        KeyboardStatusPill(
            modifier = Modifier
                .align(Alignment.TopStart)
                .padding(top = 12.dp, start = 80.dp),
        )
        TouchpadHeader(
            status = status,
            rawTouch = rawTouchMode,
            onToggleRaw = { rawTouchMode = !rawTouchMode },
            modifier = Modifier
                .align(Alignment.TopEnd)
                .padding(top = 12.dp, end = 12.dp),
        )
        Text(
            text = if (rawTouchMode) HELP_RAW else HELP_TOUCHPAD,
            color = Color(0xFF9AA5B1),
            modifier = Modifier
                .align(Alignment.BottomEnd)
                .padding(24.dp),
            style = MaterialTheme.typography.bodySmall,
        )
    }
}

@Composable
private fun TouchpadHeader(
    status: String,
    rawTouch: Boolean,
    onToggleRaw: () -> Unit,
    modifier: Modifier = Modifier,
) {
    Surface(
        modifier = modifier,
        shape = RoundedCornerShape(28.dp),
        color = MaterialTheme.colorScheme.surface.copy(alpha = 0.85f),
        tonalElevation = 4.dp,
    ) {
        Row(
            modifier = Modifier.padding(horizontal = 16.dp, vertical = 8.dp),
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(12.dp),
        ) {
            Icon(
                imageVector = Lucide.Info,
                contentDescription = null,
                tint = MaterialTheme.colorScheme.onSurfaceVariant,
                modifier = Modifier.size(18.dp),
            )
            Column {
                Text(
                    text = if (rawTouch) "Touchscreen mode" else "Touchpad mode",
                    color = MaterialTheme.colorScheme.onSurface,
                    style = MaterialTheme.typography.labelLarge,
                )
                Text(
                    text = status,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                    style = MaterialTheme.typography.bodySmall,
                )
            }
            Switch(checked = rawTouch, onCheckedChange = { onToggleRaw() })
        }
    }
}

private const val HELP_TOUCHPAD =
    "Mac-style touchpad → host clickpad.\n" +
        "tap-to-click, two-finger scroll, pinch zoom handled by libinput on the host."

private const val HELP_RAW =
    "Raw touch overlay → host MT-B Touchscreen.\n" +
        "every finger forwarded with absolute coords — gestures resolved by host apps."

// ── Constants ────────────────────────────────────────────────────────

/// Window after the pen leaves the surface during which finger /
/// palm touches are still ignored.
private const val PEN_LIFT_LATCH_MS = 250L
private const val STYLUS_ABS_MAX = 32767
private const val STYLUS_PRESSURE_MAX = 8191
private const val TOUCH_ABS_MAX = 32767

// ── Touch / stylus dispatch ──────────────────────────────────────────

private fun handlePointerEvent(
    activity: InputActivity?,
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

private fun handleTouchpadEvent(
    activity: InputActivity?,
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
    val entry = activeTouchpadTracking[pointerId] ?: return
    val slot = (entry.first and 0xFF).toByte()
    val trackingId = entry.second
    val absX = (event.getHistoricalX(idx, historyIdx).coerceIn(0f, canvas.width.toFloat()) *
        TOUCH_ABS_MAX / canvas.width).toInt().coerceIn(0, TOUCH_ABS_MAX)
    val absY = (event.getHistoricalY(idx, historyIdx).coerceIn(0f, canvas.height.toFloat()) *
        TOUCH_ABS_MAX / canvas.height).toInt().coerceIn(0, TOUCH_ABS_MAX)
    val pressure = scaleTouchpadPressure(event.getHistoricalPressure(idx, historyIdx))
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

/// Android `getPressure()` returns ~1.0 for a normal touch. Scale it
/// so that a normal touch lands at ~80 (above libinput's touch-detect
/// floor at ~30, well below its palm-reject threshold at 130).
private fun scaleTouchpadPressure(raw: Float): Int =
    (raw.coerceIn(0f, 1.5f) * 80f).toInt().coerceIn(30, 120)

private const val TOUCHPAD_SLOT_POOL = 10
private val freeTouchpadSlots = ArrayDeque<Int>().apply {
    for (i in 0 until TOUCHPAD_SLOT_POOL) add(i)
}
private val activeTouchpadTracking = HashMap<Int, Pair<Int, Int>>()
private var nextTouchpadTrackingId = 0

private fun emitTouchpadSlot(event: MotionEvent, idx: Int, canvas: IntSize, lifted: Boolean) {
    val pointerId = event.getPointerId(idx)
    val entry: Pair<Int, Int> = if (lifted) {
        val released = activeTouchpadTracking.remove(pointerId) ?: return
        freeTouchpadSlots.addLast(released.first)
        Pair(released.first, -1)
    } else {
        activeTouchpadTracking.getOrPut(pointerId) {
            val slotAssigned = freeTouchpadSlots.removeFirstOrNull() ?: return
            val tid = nextTouchpadTrackingId
            nextTouchpadTrackingId = (nextTouchpadTrackingId + 1) and 0xFFFF
            Pair(slotAssigned, tid)
        }
    }
    val slot = (entry.first and 0xFF).toByte()
    val trackingId = entry.second
    val absX = (event.getX(idx).coerceIn(0f, canvas.width.toFloat()) *
        TOUCH_ABS_MAX / canvas.width).toInt().coerceIn(0, TOUCH_ABS_MAX)
    val absY = (event.getY(idx).coerceIn(0f, canvas.height.toFloat()) *
        TOUCH_ABS_MAX / canvas.height).toInt().coerceIn(0, TOUCH_ABS_MAX)
    val pressure = scaleTouchpadPressure(event.getPressure(idx))
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

private fun handleRawTouchEvent(activity: InputActivity?, event: MotionEvent, canvas: IntSize) {
    if (canvas.width <= 0 || canvas.height <= 0) return
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

private fun scanPenIndex(event: MotionEvent): Int {
    for (i in 0 until event.pointerCount) {
        val t = event.getToolType(i)
        if (t == MotionEvent.TOOL_TYPE_STYLUS || t == MotionEvent.TOOL_TYPE_ERASER) {
            return i
        }
    }
    return -1
}

private fun isPenLatchActive(activity: InputActivity): Boolean {
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

private fun handleStylus(
    activity: InputActivity?,
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
    var btn = 0x80
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
            activity.penActive = false
            activity.penReleasedAt = SystemClock.uptimeMillis()
        } else {
            activity.penActive = true
        }
    }
    return if (eraser) "eraser p=$pressure" else "stylus p=$pressure"
}

/**
 * Hover-path stylus handler. Called from [InputActivity]'s
 * `dispatchGenericMotionEvent` override via [handleStylusHover].
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

