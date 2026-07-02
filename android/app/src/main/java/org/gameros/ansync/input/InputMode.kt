package org.gameros.ansync.input

/**
 * Which surface [InputActivity] renders. All three route to the same
 * QUIC `Input` stream via `NativeBridge.nativeSendInputMessage`; the
 * mode only controls which subset of Android events becomes wire
 * packets and how the on-screen canvas draws.
 *
 * The activity persists the last-active mode in SharedPreferences so
 * the QSTile short-tap resumes wherever the user left it. QSTiles that
 * carry an explicit `EXTRA_MODE` override the persisted default (so
 * `GamepadTile` always opens on [Gamepad] regardless of history).
 */
enum class InputMode(val wire: String) {
    Touchpad("touchpad"),
    Gamepad("gamepad");

    companion object {
        fun fromWire(s: String?): InputMode? = entries.firstOrNull { it.wire == s }
    }
}

/** Prefs keys — namespaced under the shared `ansync_prefs` file. */
const val PREF_INPUT_MODE = "input_mode"

/** JSON-encoded [GamepadLayout] override — empty = use defaults. */
const val PREF_GAMEPAD_LAYOUT = "gamepad_layout"
