package org.gameros.ansync.input

import android.view.InputDevice
import android.view.KeyEvent
import android.view.MotionEvent
import androidx.compose.foundation.Canvas
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.Button
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.ExperimentalComposeUiApi
import androidx.compose.ui.Modifier
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.drawscope.Stroke
import androidx.compose.ui.input.pointer.pointerInteropFilter
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.platform.LocalLifecycleOwner
import androidx.compose.ui.text.TextMeasurer
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.drawText
import androidx.compose.ui.text.rememberTextMeasurer
import androidx.compose.ui.unit.IntSize
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.LifecycleEventObserver
import org.gameros.ansync.KeycodeMap
import org.gameros.ansync.NativeBridge
import org.gameros.ansync.WireInputMessage

/**
 * Virtual gamepad. Renders every button + stick from a persisted
 * [GamepadLayout] and interprets on-screen touches as gamepad-button
 * / stick-drag events, aggregated into the same wire packet that
 * physical gamepad events feed.
 *
 *  ┌── Emit contract ───────────────────────────────────────────────┐
 *  │ [flushState] posts a `WireInputMessage.Gamepad` whenever ANY   │
 *  │ button bit, analog stick component, or trigger byte changes.   │
 *  │ We suppress the emit on no-op writes so idle re-renders don't  │
 *  │ produce packet floods.                                         │
 *  └────────────────────────────────────────────────────────────────┘
 *
 *  ┌── Physical passthrough ─────────────────────────────────────────┐
 *  │ [InputActivity.dispatchKeyEvent] / [dispatchGenericMotionEvent] │
 *  │ pump events into [InputActivity.gamepadEventSink]. This surface │
 *  │ installs the sink on Compose enter and clears it on leave, so   │
 *  │ Bluetooth controllers and on-screen taps land on the SAME       │
 *  │ button/stick state — they OR together into the wire packet.     │
 *  └─────────────────────────────────────────────────────────────────┘
 */
@OptIn(ExperimentalComposeUiApi::class)
@Composable
fun GamepadSurface() {
    val activity = LocalContext.current as? InputActivity
    val ctx = LocalContext.current
    val density = LocalDensity.current

    /// Persisted layout — reloaded whenever the activity resumes so
    /// the settings popup takes effect without reopening the input
    /// activity.
    var layout by remember { mutableStateOf(GamepadLayout.load(ctx)) }
    /// Snapshot taken at edit-mode entry so Cancel restores it.
    var editingSnapshot by remember { mutableStateOf<GamepadLayout?>(null) }
    /// Local flag mirroring the pref. Set from the settings popup
    /// via [PREF_GAMEPAD_EDIT_MODE]; cleared here on Save / Cancel.
    var editing by remember { mutableStateOf(editModeActive(ctx)) }
    val lifecycleOwner = LocalLifecycleOwner.current
    DisposableEffect(lifecycleOwner) {
        val obs = LifecycleEventObserver { _, event ->
            if (event == Lifecycle.Event.ON_RESUME) {
                layout = GamepadLayout.load(ctx)
                val next = editModeActive(ctx)
                if (next && !editing) {
                    editingSnapshot = layout
                }
                editing = next
            }
        }
        lifecycleOwner.lifecycle.addObserver(obs)
        onDispose { lifecycleOwner.lifecycle.removeObserver(obs) }
    }

    val state = remember { GamepadState() }

    /// Install / clear the physical-event sink. Guarded by `activity`
    /// because a preview render (no activity) still enters this
    /// composable.
    DisposableEffect(activity, state) {
        activity?.gamepadEventSink = { evt ->
            when (evt) {
                is GamepadPhysicalEvent.Key -> state.onPhysicalKey(evt.event)
                is GamepadPhysicalEvent.Motion -> state.onPhysicalMotion(evt.event)
            }
        }
        onDispose { activity?.gamepadEventSink = null }
    }

    val measurer = rememberTextMeasurer()
    var canvas by remember { mutableStateOf(IntSize.Zero) }

    /// Live edit-mode grabs: pointerId → id of the button or stick
    /// being dragged. Independent from [GamepadState.grabs] which
    /// only tracks play-mode presses.
    val editGrabs = remember { HashMap<Int, EditTarget>() }

    // Outer box carries no pointer input; canvas + overlays are its
    // siblings so overlay clicks (EditModeBar's Save/Cancel) reach
    // their own pointer handlers instead of being swallowed by the
    // canvas's `pointerInteropFilter`.
    Box(
        modifier = Modifier
            .fillMaxSize()
            .background(Color(0xFF0B0F14)),
    ) {
        Box(
            modifier = Modifier
                .fillMaxSize()
                .pointerInteropFilter { event ->
                    if (editing) {
                        layout = handleEditTouch(layout, editGrabs, event, canvas)
                    } else {
                        handleGamepadTouch(state, event, canvas, layout, density)
                    }
                    true
                },
        ) {
            Canvas(modifier = Modifier.fillMaxSize()) {
                canvas = IntSize(size.width.toInt(), size.height.toInt())
                drawGamepad(layout, state, measurer, density, editing)
            }
        }
        KeyboardStatusPill(
            modifier = Modifier
                .align(Alignment.TopStart)
                .padding(top = 12.dp, start = 80.dp),
        )
        if (editing) {
            EditModeBar(
                onCancel = {
                    val snap = editingSnapshot
                    if (snap != null) {
                        layout = snap
                        snap.persist(ctx)
                    }
                    editingSnapshot = null
                    setEditMode(ctx, false)
                    editing = false
                },
                onSave = {
                    layout.persist(ctx)
                    editingSnapshot = null
                    setEditMode(ctx, false)
                    editing = false
                },
                modifier = Modifier
                    .align(Alignment.TopCenter)
                    .padding(top = 12.dp),
            )
        }
    }
}

@Composable
private fun EditModeBar(
    onCancel: () -> Unit,
    onSave: () -> Unit,
    modifier: Modifier = Modifier,
) {
    Surface(
        modifier = modifier,
        shape = RoundedCornerShape(28.dp),
        color = MaterialTheme.colorScheme.surface.copy(alpha = 0.92f),
        tonalElevation = 6.dp,
    ) {
        Row(
            modifier = Modifier.padding(horizontal = 16.dp, vertical = 8.dp),
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(12.dp),
        ) {
            Text(
                text = "Drag any control to reposition",
                color = MaterialTheme.colorScheme.onSurface,
                style = MaterialTheme.typography.labelLarge,
            )
            TextButton(onClick = onCancel) { Text("Cancel") }
            Button(onClick = onSave) { Text("Save") }
        }
    }
}

// ── Edit-mode dispatch ───────────────────────────────────────────────

/**
 * Which layout entry a pointer is currently dragging. Buttons and
 * sticks share the pool because a pointer can only own one control
 * at a time.
 */
internal sealed class EditTarget {
    data class Button(val id: GamepadButton) : EditTarget()
    data class Stick(val side: GamepadStick) : EditTarget()
}

private fun handleEditTouch(
    layout: GamepadLayout,
    grabs: HashMap<Int, EditTarget>,
    event: MotionEvent,
    canvas: IntSize,
): GamepadLayout {
    if (canvas.width <= 0 || canvas.height <= 0) return layout
    var next = layout
    when (event.actionMasked) {
        MotionEvent.ACTION_DOWN, MotionEvent.ACTION_POINTER_DOWN -> {
            val idx = event.actionIndex
            val pid = event.getPointerId(idx)
            val target = pickEditTarget(event.getX(idx), event.getY(idx), canvas, next)
            if (target != null) grabs[pid] = target
        }
        MotionEvent.ACTION_MOVE -> {
            for (i in 0 until event.pointerCount) {
                val pid = event.getPointerId(i)
                val target = grabs[pid] ?: continue
                val fx = (event.getX(i) / canvas.width).coerceIn(0f, 1f)
                val fy = (event.getY(i) / canvas.height).coerceIn(0f, 1f)
                when (target) {
                    is EditTarget.Button -> {
                        val p = next.buttons[target.id] ?: continue
                        next = next.withButton(target.id, p.copy(cx = fx, cy = fy))
                    }
                    is EditTarget.Stick -> {
                        val p = next.sticks[target.side] ?: continue
                        next = next.withStick(target.side, p.copy(cx = fx, cy = fy))
                    }
                }
            }
        }
        MotionEvent.ACTION_UP, MotionEvent.ACTION_POINTER_UP -> {
            val idx = event.actionIndex
            val pid = event.getPointerId(idx)
            grabs.remove(pid)
        }
        MotionEvent.ACTION_CANCEL -> grabs.clear()
    }
    return next
}

private fun pickEditTarget(
    x: Float,
    y: Float,
    canvas: IntSize,
    layout: GamepadLayout,
): EditTarget? {
    // Buttons first in edit mode — smaller targets need the click,
    // sticks get everything not claimed by a button (mirror image of
    // play-mode priority).
    var best: Pair<Float, EditTarget>? = null
    for ((id, p) in layout.buttons) {
        val cx = p.cx * canvas.width
        val cy = p.cy * canvas.height
        val d = (x - cx) * (x - cx) + (y - cy) * (y - cy)
        val r = (p.radius * 3f)   // fatter grab area vs. play radius (dp*3 ≈ px slack)
        if (d < r * r && (best == null || d < best.first)) {
            best = d to EditTarget.Button(id)
        }
    }
    if (best != null) return best.second
    for ((side, p) in layout.sticks) {
        val cx = p.cx * canvas.width
        val cy = p.cy * canvas.height
        val d = (x - cx) * (x - cx) + (y - cy) * (y - cy)
        val r = p.outerRadius * 3f
        if (d < r * r) return EditTarget.Stick(side)
    }
    return null
}

// ── Aggregated state ─────────────────────────────────────────────────

/**
 * Live gamepad state. Fields are volatile-ish through
 * `mutableStateOf` so the renderer recomposes on any update, and
 * `emit()` is invoked on every mutation to push a wire packet.
 *
 * Concurrent writes are serialised implicitly: the touch dispatcher
 * runs on the main thread; the physical-event sink is also invoked
 * on the main thread by [InputActivity.dispatchKeyEvent] /
 * `dispatchGenericMotionEvent`.
 */
internal class GamepadState {
    var buttons: Int by mutableStateOf(0)
        private set
    var lx: Short by mutableStateOf(0)
        private set
    var ly: Short by mutableStateOf(0)
        private set
    var rx: Short by mutableStateOf(0)
        private set
    var ry: Short by mutableStateOf(0)
        private set
    var lt: Byte by mutableStateOf(0)
        private set
    var rt: Byte by mutableStateOf(0)
        private set

    /**
     * Live pointer → touch-grab map. Populated by the touch
     * dispatcher; each entry drives one interaction (button press or
     * stick drag).
     */
    val grabs = HashMap<Int, TouchGrab>()

    /**
     * Physical-side pressed bitmask. Kept separate from touch presses
     * so a physical button-hold OR an on-screen tap on the same
     * button both count — release of either restores the underlying
     * bit only when both sources have released.
     */
    private var physicalButtons: Int = 0
    private var touchButtons: Int = 0

    fun pressTouch(id: GamepadButton) {
        val bit = id.bit
        if (bit == null) {
            when (id) {
                GamepadButton.L2 -> { lt = 0xFF.toByte(); emit() }
                GamepadButton.R2 -> { rt = 0xFF.toByte(); emit() }
                else -> Unit
            }
            return
        }
        touchButtons = touchButtons or (1 shl bit)
        rebuildAndEmit()
    }

    fun releaseTouch(id: GamepadButton) {
        val bit = id.bit
        if (bit == null) {
            when (id) {
                GamepadButton.L2 -> { lt = 0; emit() }
                GamepadButton.R2 -> { rt = 0; emit() }
                else -> Unit
            }
            return
        }
        touchButtons = touchButtons and (1 shl bit).inv()
        rebuildAndEmit()
    }

    fun setStickTouch(side: GamepadStick, xNorm: Float, yNorm: Float) {
        val x = (xNorm.coerceIn(-1f, 1f) * Short.MAX_VALUE.toFloat()).toInt()
            .coerceIn(Short.MIN_VALUE.toInt(), Short.MAX_VALUE.toInt())
            .toShort()
        val y = (yNorm.coerceIn(-1f, 1f) * Short.MAX_VALUE.toFloat()).toInt()
            .coerceIn(Short.MIN_VALUE.toInt(), Short.MAX_VALUE.toInt())
            .toShort()
        val changed = when (side) {
            GamepadStick.L -> {
                val c = lx != x || ly != y
                lx = x; ly = y
                c
            }
            GamepadStick.R -> {
                val c = rx != x || ry != y
                rx = x; ry = y
                c
            }
        }
        if (changed) emit()
    }

    fun resetStickTouch(side: GamepadStick) {
        val changed = when (side) {
            GamepadStick.L -> (lx != 0.toShort() || ly != 0.toShort()).also { lx = 0; ly = 0 }
            GamepadStick.R -> (rx != 0.toShort() || ry != 0.toShort()).also { rx = 0; ry = 0 }
        }
        if (changed) emit()
    }

    fun onPhysicalKey(event: KeyEvent): Boolean {
        val bit = KeycodeMap.toGamepadButtonBit(event.keyCode)
        if (bit != null) {
            val mask = 1 shl bit
            physicalButtons = if (event.action == KeyEvent.ACTION_DOWN) {
                physicalButtons or mask
            } else {
                physicalButtons and mask.inv()
            }
            rebuildAndEmit()
            return true
        }
        // L2 / R2 as discrete keyevents (some controllers).
        when (event.keyCode) {
            KeyEvent.KEYCODE_BUTTON_L2 -> {
                lt = if (event.action == KeyEvent.ACTION_DOWN) 0xFF.toByte() else 0
                emit(); return true
            }
            KeyEvent.KEYCODE_BUTTON_R2 -> {
                rt = if (event.action == KeyEvent.ACTION_DOWN) 0xFF.toByte() else 0
                emit(); return true
            }
        }
        return false
    }

    fun onPhysicalMotion(event: MotionEvent): Boolean {
        if ((event.source and InputDevice.SOURCE_JOYSTICK) != InputDevice.SOURCE_JOYSTICK) {
            return false
        }
        if (event.actionMasked != MotionEvent.ACTION_MOVE) return false
        val newLx = axisShort(event, MotionEvent.AXIS_X)
        val newLy = axisShort(event, MotionEvent.AXIS_Y)
        val newRx = axisShort(event, MotionEvent.AXIS_Z)
        val newRy = axisShort(event, MotionEvent.AXIS_RZ)
        val newLt = triggerByte(event, MotionEvent.AXIS_LTRIGGER, MotionEvent.AXIS_BRAKE)
        val newRt = triggerByte(event, MotionEvent.AXIS_RTRIGGER, MotionEvent.AXIS_GAS)
        val changed = lx != newLx || ly != newLy || rx != newRx || ry != newRy ||
            lt != newLt || rt != newRt
        lx = newLx; ly = newLy; rx = newRx; ry = newRy; lt = newLt; rt = newRt
        if (changed) emit()
        // DPAD hat axes — most controllers surface the cross via
        // `AXIS_HAT_X` (-1 left, +1 right) and `AXIS_HAT_Y` (-1 up,
        // +1 down) instead of discrete key events. Translate into
        // bits 11-14 of the physical mask.
        val hatX = event.getAxisValue(MotionEvent.AXIS_HAT_X)
        val hatY = event.getAxisValue(MotionEvent.AXIS_HAT_Y)
        val dpadMask =
            (if (hatY < -0.5f) 1 shl 11 else 0) or   // Up
            (if (hatY > 0.5f) 1 shl 12 else 0) or    // Down
            (if (hatX < -0.5f) 1 shl 13 else 0) or   // Left
            (if (hatX > 0.5f) 1 shl 14 else 0)       // Right
        val dpadBits = (1 shl 11) or (1 shl 12) or (1 shl 13) or (1 shl 14)
        val newPhysical = (physicalButtons and dpadBits.inv()) or dpadMask
        if (newPhysical != physicalButtons) {
            physicalButtons = newPhysical
            rebuildAndEmit()
        }
        return true
    }

    private fun rebuildAndEmit() {
        val next = physicalButtons or touchButtons
        if (next != buttons) {
            buttons = next
            emit()
        }
    }

    private fun emit() {
        NativeBridge.nativeSendInputMessage(
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
    }
}

private fun axisShort(event: MotionEvent, axis: Int): Short {
    val raw = event.getAxisValue(axis).coerceIn(-1f, 1f)
    return (raw * Short.MAX_VALUE.toFloat()).toInt()
        .coerceIn(Short.MIN_VALUE.toInt(), Short.MAX_VALUE.toInt())
        .toShort()
}

private fun triggerByte(event: MotionEvent, primary: Int, fallback: Int): Byte {
    var v = event.getAxisValue(primary)
    if (v == 0f) v = event.getAxisValue(fallback)
    val clamped = (v.coerceIn(0f, 1f) * 255f).toInt().coerceIn(0, 255)
    return clamped.toByte()
}

// ── Touch → button/stick dispatch ────────────────────────────────────

/**
 * One pointerId's current grab. Buttons are stateless (press / release
 * on down / up); sticks track the base center so deltas map cleanly to
 * axis output regardless of where the finger started.
 */
internal sealed class TouchGrab {
    data class Button(val id: GamepadButton) : TouchGrab()
    data class Stick(
        val side: GamepadStick,
        val cx: Float,
        val cy: Float,
        val outerPx: Float,
    ) : TouchGrab()
}

private fun handleGamepadTouch(
    state: GamepadState,
    event: MotionEvent,
    canvas: IntSize,
    layout: GamepadLayout,
    density: androidx.compose.ui.unit.Density,
) {
    if (canvas.width <= 0 || canvas.height <= 0) return
    when (event.actionMasked) {
        MotionEvent.ACTION_DOWN, MotionEvent.ACTION_POINTER_DOWN -> {
            val idx = event.actionIndex
            val pid = event.getPointerId(idx)
            val grab = pickGrab(event.getX(idx), event.getY(idx), canvas, layout, density)
            if (grab != null) {
                state.grabs[pid] = grab
                applyGrabDown(state, grab, event.getX(idx), event.getY(idx))
            }
        }
        MotionEvent.ACTION_MOVE -> {
            for (i in 0 until event.pointerCount) {
                val pid = event.getPointerId(i)
                val grab = state.grabs[pid] ?: continue
                if (grab is TouchGrab.Stick) {
                    applyStickMove(state, grab, event.getX(i), event.getY(i))
                }
            }
        }
        MotionEvent.ACTION_UP, MotionEvent.ACTION_POINTER_UP -> {
            val idx = event.actionIndex
            val pid = event.getPointerId(idx)
            val grab = state.grabs.remove(pid) ?: return
            applyGrabUp(state, grab)
        }
        MotionEvent.ACTION_CANCEL -> {
            for ((_, grab) in state.grabs) applyGrabUp(state, grab)
            state.grabs.clear()
        }
    }
}

private fun pickGrab(
    x: Float,
    y: Float,
    canvas: IntSize,
    layout: GamepadLayout,
    density: androidx.compose.ui.unit.Density,
): TouchGrab? {
    // Sticks first — their catchment area is bigger, so they take
    // priority over a stray button under the outer ring.
    for ((side, p) in layout.sticks) {
        val cx = p.cx * canvas.width
        val cy = p.cy * canvas.height
        val r = with(density) { p.outerRadius.dp.toPx() }
        val dx = x - cx; val dy = y - cy
        if (dx * dx + dy * dy <= r * r) {
            return TouchGrab.Stick(side, cx, cy, r)
        }
    }
    for ((id, p) in layout.buttons) {
        val cx = p.cx * canvas.width
        val cy = p.cy * canvas.height
        val r = with(density) { p.radius.dp.toPx() }
        val dx = x - cx; val dy = y - cy
        if (dx * dx + dy * dy <= r * r) {
            return TouchGrab.Button(id)
        }
    }
    return null
}

private fun applyGrabDown(state: GamepadState, grab: TouchGrab, x: Float, y: Float) {
    when (grab) {
        is TouchGrab.Button -> state.pressTouch(grab.id)
        is TouchGrab.Stick -> applyStickMove(state, grab, x, y)
    }
}

private fun applyGrabUp(state: GamepadState, grab: TouchGrab) {
    when (grab) {
        is TouchGrab.Button -> state.releaseTouch(grab.id)
        is TouchGrab.Stick -> state.resetStickTouch(grab.side)
    }
}

private fun applyStickMove(state: GamepadState, grab: TouchGrab.Stick, x: Float, y: Float) {
    var dx = x - grab.cx
    var dy = y - grab.cy
    val dist = kotlin.math.sqrt(dx * dx + dy * dy)
    if (dist > grab.outerPx) {
        val k = grab.outerPx / dist
        dx *= k; dy *= k
    }
    // Y-down in Android canvas coords; XInput convention has Y-up
    // positive when the stick pushes forward. Flip Y so pushing the
    // finger up (visually) reads as ly ~ -32768 (matches physical
    // stick convention).
    val nx = dx / grab.outerPx
    val ny = dy / grab.outerPx
    state.setStickTouch(grab.side, nx, ny)
}

// ── Rendering ────────────────────────────────────────────────────────

private fun androidx.compose.ui.graphics.drawscope.DrawScope.drawGamepad(
    layout: GamepadLayout,
    state: GamepadState,
    measurer: TextMeasurer,
    density: androidx.compose.ui.unit.Density,
    editing: Boolean,
) {
    val w = size.width
    val h = size.height
    val labelStyle = TextStyle(
        color = Color.White.copy(alpha = 0.85f),
        fontSize = 12.sp,
    )
    // In edit mode we outline every control with a dashed accent so
    // the user can see the grabbable region even when a button is
    // "released" (drawn dimly in play mode).
    val editStroke = Color(0xFFFFB74D)
    for ((id, p) in layout.buttons) {
        val cx = p.cx * w
        val cy = p.cy * h
        val r = with(density) { p.radius.dp.toPx() }
        val pressed = when (id) {
            GamepadButton.L2 -> (state.lt.toInt() and 0xFF) > 0
            GamepadButton.R2 -> (state.rt.toInt() and 0xFF) > 0
            else -> id.bit?.let { (state.buttons and (1 shl it)) != 0 } == true
        }
        val fill = buttonTint(id).copy(alpha = if (pressed) p.alpha else p.alpha * 0.45f)
        val stroke = Color.White.copy(alpha = p.alpha)
        drawCircle(color = fill, radius = r, center = Offset(cx, cy))
        drawCircle(color = stroke, radius = r, center = Offset(cx, cy),
            style = Stroke(width = with(density) { 1.5.dp.toPx() }))
        if (editing) {
            drawCircle(
                color = editStroke,
                radius = r + with(density) { 3.dp.toPx() },
                center = Offset(cx, cy),
                style = Stroke(width = with(density) { 2.dp.toPx() }),
            )
        }
        val text = measurer.measure(id.label, style = labelStyle)
        drawText(
            textLayoutResult = text,
            topLeft = Offset(cx - text.size.width / 2f, cy - text.size.height / 2f),
        )
    }
    for ((side, p) in layout.sticks) {
        val cx = p.cx * w
        val cy = p.cy * h
        val outer = with(density) { p.outerRadius.dp.toPx() }
        val thumb = with(density) { p.thumbRadius.dp.toPx() }
        val nx = when (side) {
            GamepadStick.L -> state.lx.toInt() / Short.MAX_VALUE.toFloat()
            GamepadStick.R -> state.rx.toInt() / Short.MAX_VALUE.toFloat()
        }
        val ny = when (side) {
            GamepadStick.L -> state.ly.toInt() / Short.MAX_VALUE.toFloat()
            GamepadStick.R -> state.ry.toInt() / Short.MAX_VALUE.toFloat()
        }
        // Base ring.
        drawCircle(
            color = Color.White.copy(alpha = p.alpha * 0.15f),
            radius = outer,
            center = Offset(cx, cy),
        )
        drawCircle(
            color = Color.White.copy(alpha = p.alpha),
            radius = outer,
            center = Offset(cx, cy),
            style = Stroke(width = with(density) { 1.5.dp.toPx() }),
        )
        // Thumb.
        val tx = cx + nx * (outer - thumb)
        val ty = cy + ny * (outer - thumb)
        drawCircle(
            color = Color(0xFF6EC1E4).copy(alpha = p.alpha),
            radius = thumb,
            center = Offset(tx, ty),
        )
        if (editing) {
            drawCircle(
                color = editStroke,
                radius = outer + with(density) { 3.dp.toPx() },
                center = Offset(cx, cy),
                style = Stroke(width = with(density) { 2.dp.toPx() }),
            )
        }
    }
}

private fun buttonTint(id: GamepadButton): Color = when (id) {
    GamepadButton.A -> Color(0xFF4CAF50)      // green (Xbox / Nintendo A)
    GamepadButton.B -> Color(0xFFE53935)      // red
    GamepadButton.X -> Color(0xFF1E88E5)      // blue
    GamepadButton.Y -> Color(0xFFFDD835)      // yellow
    GamepadButton.L1, GamepadButton.R1,
    GamepadButton.L2, GamepadButton.R2 -> Color(0xFF7C4DFF)   // shoulders / triggers
    GamepadButton.Start, GamepadButton.Select, GamepadButton.Mode -> Color(0xFF9E9E9E)
    GamepadButton.ThumbL, GamepadButton.ThumbR -> Color(0xFF6EC1E4)
    GamepadButton.DpadUp, GamepadButton.DpadDown,
    GamepadButton.DpadLeft, GamepadButton.DpadRight -> Color(0xFFB0BEC5)
}
