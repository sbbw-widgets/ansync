package org.gameros.ansync.input

import android.content.Intent
import android.content.res.Configuration
import android.os.Bundle
import android.os.SystemClock
import android.view.InputDevice
import android.view.KeyEvent
import android.view.MotionEvent
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.darkColorScheme
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import org.gameros.ansync.PREFS

/**
 * Unified device→host input surface. Three modes (Touchpad, Keyboard,
 * Gamepad) share the same activity, the same QUIC `Input` stream, and
 * the same left-edge floating rail for mode-switching. Users flip
 * modes on the fly without leaving the activity.
 *
 * The Activity owns the routing decisions for events that are only
 * observable at Activity scope:
 *
 *  - [dispatchKeyEvent] classifies gamepad-source key events → virtual
 *    gamepad state (when the mode is [InputMode.Gamepad]); everything
 *    else is forwarded to the host over the shared uinput keyboard.
 *  - [dispatchGenericMotionEvent] routes joystick MotionEvents to the
 *    gamepad state when Gamepad mode is active, and stylus hover
 *    events to the touchpad pipeline otherwise.
 *
 * The pen-palm latch state ([penActive], [penReleasedAt]) is scoped to
 * the touchpad surface but stays on the Activity so hover events that
 * fire before the composable can install its `pointerInteropFilter`
 * still land on the latch fields.
 */
class InputActivity : ComponentActivity() {

    /// Canvas rect in window coords for the touchpad surface. The
    /// composable writes these via `onGloballyPositioned` /
    /// `onSizeChanged`; [dispatchGenericMotionEvent] reads them to
    /// translate stylus hover events (which arrive at the Activity
    /// before the View tree, *not* through `pointerInteropFilter`).
    @Volatile var canvasLeft: Float = 0f
    @Volatile var canvasTop: Float = 0f
    @Volatile var canvasWidth: Int = 0
    @Volatile var canvasHeight: Int = 0

    /// Pen-palm rejection state for the touchpad surface. See
    /// [TouchpadSurface] for the full description.
    @Volatile var penActive: Boolean = false
    @Volatile var penReleasedAt: Long = 0L

    /// Currently active mode. Volatile because both dispatch overrides
    /// (Activity thread) and the virtual gamepad renderer (UI thread)
    /// read it without a lock.
    @Volatile private var activeMode: InputMode = InputMode.Touchpad

    /// State delegated by [GamepadSurface] — the surface installs a
    /// pump into this slot on Compose enter and clears it on leave.
    /// Physical gamepad events dispatched at the Activity level route
    /// through it so the wire packets track a unified button/stick
    /// state regardless of whether the input came from an on-screen
    /// tap or a Bluetooth controller.
    @Volatile var gamepadEventSink: ((GamepadPhysicalEvent) -> Boolean)? = null

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        val explicit = InputMode.fromWire(intent.getStringExtra(EXTRA_MODE))
        val persisted = InputMode.fromWire(
            getSharedPreferences(PREFS, MODE_PRIVATE).getString(PREF_INPUT_MODE, null)
        )
        activeMode = explicit ?: persisted ?: InputMode.Touchpad

        setContent {
            MaterialTheme(colorScheme = darkColorScheme()) {
                InputScaffold(
                    initialMode = activeMode,
                    onModeChanged = { mode ->
                        activeMode = mode
                        getSharedPreferences(PREFS, MODE_PRIVATE)
                            .edit()
                            .putString(PREF_INPUT_MODE, mode.wire)
                            .apply()
                    },
                    onOpenSettings = {
                        startActivity(
                            Intent(this, InputSettingsActivity::class.java)
                                .putExtra(InputSettingsActivity.EXTRA_MODE, activeMode.wire)
                        )
                    },
                )
            }
        }
    }

    override fun dispatchKeyEvent(event: KeyEvent): Boolean {
        // Route gamepad-source keys straight to the virtual gamepad
        // surface *only* when Gamepad mode is active. In the other
        // modes the user is not focused on the pad; the events should
        // still reach the host as if the Bluetooth controller were
        // physically wired to the daemon.
        val gamepadSource = (event.source and InputDevice.SOURCE_GAMEPAD) ==
            InputDevice.SOURCE_GAMEPAD ||
            (event.source and InputDevice.SOURCE_JOYSTICK) ==
            InputDevice.SOURCE_JOYSTICK
        if (gamepadSource && activeMode == InputMode.Gamepad) {
            val sink = gamepadEventSink
            if (sink != null && sink(GamepadPhysicalEvent.Key(event))) return true
        }
        // Everything else (or when no sink is installed) falls back
        // to the shared keyboard forwarder used by the touchpad +
        // keyboard modes.
        return handleKeyForHost(event) || super.dispatchKeyEvent(event)
    }

    override fun dispatchGenericMotionEvent(event: MotionEvent): Boolean {
        val isJoystick = (event.source and InputDevice.SOURCE_JOYSTICK) ==
            InputDevice.SOURCE_JOYSTICK
        if (isJoystick && activeMode == InputMode.Gamepad) {
            val sink = gamepadEventSink
            if (sink != null && sink(GamepadPhysicalEvent.Motion(event))) return true
        }
        // Stylus hover only makes sense in touchpad mode — the
        // keyboard / gamepad surfaces don't paint a canvas the pen
        // could hover over.
        if (activeMode == InputMode.Touchpad) {
            if (handleStylusHover(event)) return true
        }
        return super.dispatchGenericMotionEvent(event)
    }

    companion object {
        /** Optional intent extra to force an initial mode. QSTiles set
         *  this so `TouchpadTile` and `GamepadTile` land on their
         *  respective surfaces regardless of the persisted default. */
        const val EXTRA_MODE = "mode"
    }
}

/**
 * Discriminated union for physical gamepad events pumped into
 * [GamepadSurface]. The composable stores the sink on the Activity so
 * both on-screen (tap) and off-screen (Bluetooth controller) inputs
 * can converge into the same aggregated wire packet.
 */
sealed class GamepadPhysicalEvent {
    data class Key(val event: KeyEvent) : GamepadPhysicalEvent()
    data class Motion(val event: MotionEvent) : GamepadPhysicalEvent()
}

@androidx.compose.runtime.Composable
private fun InputScaffold(
    initialMode: InputMode,
    onModeChanged: (InputMode) -> Unit,
    onOpenSettings: () -> Unit,
) {
    var mode by remember { mutableStateOf(initialMode) }
    /// Soft-IME visibility. Rail's keyboard button flips it; the
    /// scaffold-scoped [IMESink] mounts an offscreen EditText whose
    /// InputConnection routes commits to the host as evdev keys.
    var imeOpen by remember { mutableStateOf(false) }
    LaunchedEffect(mode) { onModeChanged(mode) }
    Box(
        Modifier
            .fillMaxSize()
            .background(Color(0xFF0B0F14)),
    ) {
        // Surface fills the whole activity; rail floats on top on the
        // left edge. Surfaces MUST NOT reserve horizontal space for
        // the rail — the touchpad canvas covers the whole area behind
        // it so gestures that start under the rail still track.
        when (mode) {
            InputMode.Touchpad -> TouchpadSurface()
            InputMode.Gamepad -> GamepadSurface()
        }
        // Offscreen IME sink — always mounted so the show / hide
        // transition is a single IMM call rather than a factory bounce.
        IMESink(open = imeOpen)
        InputRail(
            mode = mode,
            imeOpen = imeOpen,
            onSelectMode = { mode = it },
            onToggleKeyboard = { imeOpen = !imeOpen },
            onOpenSettings = onOpenSettings,
            modifier = Modifier.align(Alignment.CenterStart),
        )
    }
}

// ── Shared keyboard forwarder used by touchpad + keyboard modes ──────

/**
 * True if [event] should be consumed by the host-forwarding path.
 * Returns `false` for gamepad-source events (they may be handled by
 * the virtual gamepad surface) and for the synthesised IME events
 * whose deviceId is [KeyEvent.KEYCODE_UNKNOWN] (those go through the
 * `HostKeyboardEditText` `InputConnection` instead).
 */
internal fun handleKeyForHost(event: KeyEvent): Boolean {
    if ((event.source and InputDevice.SOURCE_GAMEPAD) == InputDevice.SOURCE_GAMEPAD ||
        (event.source and InputDevice.SOURCE_JOYSTICK) == InputDevice.SOURCE_JOYSTICK
    ) {
        return false
    }
    if (event.deviceId == KeyEvent.KEYCODE_UNKNOWN) return false
    val evdev = org.gameros.ansync.KeycodeMap.toEvdev(event.keyCode) ?: return false
    if (event.action == KeyEvent.ACTION_DOWN || event.action == KeyEvent.ACTION_UP) {
        sendKeyToHost(evdev, event.action == KeyEvent.ACTION_DOWN)
        return true
    }
    return false
}

internal fun sendKeyToHost(evdev: Int, pressed: Boolean) {
    org.gameros.ansync.NativeBridge.nativeSendInputMessage(
        org.gameros.ansync.WireInputMessage.KeyPress(keycode = evdev, pressed = pressed).encode()
    )
}

/**
 * Stylus-hover routing for the touchpad surface. Called from the
 * activity-level [InputActivity.dispatchGenericMotionEvent]. Returns
 * `true` when the event was fully consumed (pen inside the canvas).
 */
internal fun InputActivity.handleStylusHover(event: MotionEvent): Boolean {
    val tool = if (event.pointerCount > 0) event.getToolType(0) else MotionEvent.TOOL_TYPE_UNKNOWN
    val isPen = tool == MotionEvent.TOOL_TYPE_STYLUS || tool == MotionEvent.TOOL_TYPE_ERASER
    if (!isPen) return false
    if (canvasWidth <= 0 || canvasHeight <= 0) return false
    when (event.actionMasked) {
        MotionEvent.ACTION_HOVER_ENTER,
        MotionEvent.ACTION_HOVER_MOVE,
        MotionEvent.ACTION_HOVER_EXIT,
        -> {
            val localX = event.x - canvasLeft
            val localY = event.y - canvasTop
            if (localX < 0f || localY < 0f ||
                localX > canvasWidth || localY > canvasHeight
            ) return false
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
    return false
}

/** Whether the device currently reports an attached hardware keyboard.
 *  Consumed by [KeyboardStatusPill] to render the plugged / unplugged
 *  state indicator. */
internal fun ComponentActivity.hasHardwareKeyboard(): Boolean {
    val cfg = resources.configuration
    return cfg.keyboard != Configuration.KEYBOARD_NOKEYS &&
        cfg.hardKeyboardHidden != Configuration.HARDKEYBOARDHIDDEN_YES
}
