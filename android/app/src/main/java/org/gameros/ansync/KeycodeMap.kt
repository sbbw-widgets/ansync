package org.gameros.ansync

import android.view.KeyEvent

/**
 * Android `KeyEvent.KEYCODE_*` → Linux `<linux/input-event-codes.h>`
 * `KEY_*` evdev codes. The host side `ansync_input::uinput::Keyboard`
 * writes `keycode as u16` straight to the kernel, so the value must
 * already be in the evdev numbering space when it leaves the
 * companion.
 *
 * Returns `null` for keys that have no straightforward mapping
 * (multimedia, IME composition keys, etc.) — callers should drop
 * those events rather than forwarding garbage.
 *
 * Modifier state (shift / ctrl / alt / meta) is forwarded as
 * separate `KEY_*` press/release events: the host uinput keyboard
 * advertises every evdev key code, so the modifier stickiness is
 * tracked by the kernel input layer on the receiving side.
 */
object KeycodeMap {
    fun toEvdev(androidKeycode: Int): Int? = when (androidKeycode) {
        // letters
        KeyEvent.KEYCODE_A -> 30
        KeyEvent.KEYCODE_B -> 48
        KeyEvent.KEYCODE_C -> 46
        KeyEvent.KEYCODE_D -> 32
        KeyEvent.KEYCODE_E -> 18
        KeyEvent.KEYCODE_F -> 33
        KeyEvent.KEYCODE_G -> 34
        KeyEvent.KEYCODE_H -> 35
        KeyEvent.KEYCODE_I -> 23
        KeyEvent.KEYCODE_J -> 36
        KeyEvent.KEYCODE_K -> 37
        KeyEvent.KEYCODE_L -> 38
        KeyEvent.KEYCODE_M -> 50
        KeyEvent.KEYCODE_N -> 49
        KeyEvent.KEYCODE_O -> 24
        KeyEvent.KEYCODE_P -> 25
        KeyEvent.KEYCODE_Q -> 16
        KeyEvent.KEYCODE_R -> 19
        KeyEvent.KEYCODE_S -> 31
        KeyEvent.KEYCODE_T -> 20
        KeyEvent.KEYCODE_U -> 22
        KeyEvent.KEYCODE_V -> 47
        KeyEvent.KEYCODE_W -> 17
        KeyEvent.KEYCODE_X -> 45
        KeyEvent.KEYCODE_Y -> 21
        KeyEvent.KEYCODE_Z -> 44

        // digits row
        KeyEvent.KEYCODE_1 -> 2
        KeyEvent.KEYCODE_2 -> 3
        KeyEvent.KEYCODE_3 -> 4
        KeyEvent.KEYCODE_4 -> 5
        KeyEvent.KEYCODE_5 -> 6
        KeyEvent.KEYCODE_6 -> 7
        KeyEvent.KEYCODE_7 -> 8
        KeyEvent.KEYCODE_8 -> 9
        KeyEvent.KEYCODE_9 -> 10
        KeyEvent.KEYCODE_0 -> 11

        // editing
        KeyEvent.KEYCODE_ENTER, KeyEvent.KEYCODE_NUMPAD_ENTER -> 28
        KeyEvent.KEYCODE_DEL -> 14            // backspace
        KeyEvent.KEYCODE_FORWARD_DEL -> 111   // delete
        KeyEvent.KEYCODE_TAB -> 15
        KeyEvent.KEYCODE_SPACE -> 57
        KeyEvent.KEYCODE_ESCAPE -> 1

        // punctuation row (US ANSI layout)
        KeyEvent.KEYCODE_MINUS -> 12
        KeyEvent.KEYCODE_EQUALS -> 13
        KeyEvent.KEYCODE_LEFT_BRACKET -> 26
        KeyEvent.KEYCODE_RIGHT_BRACKET -> 27
        KeyEvent.KEYCODE_BACKSLASH -> 43
        KeyEvent.KEYCODE_SEMICOLON -> 39
        KeyEvent.KEYCODE_APOSTROPHE -> 40
        KeyEvent.KEYCODE_GRAVE -> 41
        KeyEvent.KEYCODE_COMMA -> 51
        KeyEvent.KEYCODE_PERIOD -> 52
        KeyEvent.KEYCODE_SLASH -> 53

        // modifiers
        KeyEvent.KEYCODE_SHIFT_LEFT -> 42
        KeyEvent.KEYCODE_SHIFT_RIGHT -> 54
        KeyEvent.KEYCODE_CTRL_LEFT -> 29
        KeyEvent.KEYCODE_CTRL_RIGHT -> 97
        KeyEvent.KEYCODE_ALT_LEFT -> 56
        KeyEvent.KEYCODE_ALT_RIGHT -> 100
        KeyEvent.KEYCODE_META_LEFT -> 125
        KeyEvent.KEYCODE_META_RIGHT -> 126
        KeyEvent.KEYCODE_CAPS_LOCK -> 58

        // arrows + nav
        KeyEvent.KEYCODE_DPAD_UP -> 103
        KeyEvent.KEYCODE_DPAD_LEFT -> 105
        KeyEvent.KEYCODE_DPAD_RIGHT -> 106
        KeyEvent.KEYCODE_DPAD_DOWN -> 108
        KeyEvent.KEYCODE_MOVE_HOME -> 102
        KeyEvent.KEYCODE_MOVE_END -> 107
        KeyEvent.KEYCODE_PAGE_UP -> 104
        KeyEvent.KEYCODE_PAGE_DOWN -> 109
        KeyEvent.KEYCODE_INSERT -> 110

        // function row
        KeyEvent.KEYCODE_F1 -> 59
        KeyEvent.KEYCODE_F2 -> 60
        KeyEvent.KEYCODE_F3 -> 61
        KeyEvent.KEYCODE_F4 -> 62
        KeyEvent.KEYCODE_F5 -> 63
        KeyEvent.KEYCODE_F6 -> 64
        KeyEvent.KEYCODE_F7 -> 65
        KeyEvent.KEYCODE_F8 -> 66
        KeyEvent.KEYCODE_F9 -> 67
        KeyEvent.KEYCODE_F10 -> 68
        KeyEvent.KEYCODE_F11 -> 87
        KeyEvent.KEYCODE_F12 -> 88

        else -> null
    }

    /**
     * Android gamepad button [KeyEvent] keycode → bit position in the
     * `GamepadState.buttons` 32-bit mask. The bit layout mirrors the
     * standard XInput-style ordering used by the host uinput
     * `Gamepad` device.
     *
     * Returns `null` for non-gamepad keys.
     */
    fun toGamepadButtonBit(androidKeycode: Int): Int? = when (androidKeycode) {
        KeyEvent.KEYCODE_BUTTON_A -> 0          // ButtonSouth
        KeyEvent.KEYCODE_BUTTON_B -> 1          // ButtonEast
        KeyEvent.KEYCODE_BUTTON_Y -> 2          // ButtonNorth
        KeyEvent.KEYCODE_BUTTON_X -> 3          // ButtonWest
        KeyEvent.KEYCODE_BUTTON_L1 -> 4         // TL
        KeyEvent.KEYCODE_BUTTON_R1 -> 5         // TR
        KeyEvent.KEYCODE_BUTTON_SELECT -> 6
        KeyEvent.KEYCODE_BUTTON_START -> 7
        KeyEvent.KEYCODE_BUTTON_MODE -> 8
        KeyEvent.KEYCODE_BUTTON_THUMBL -> 9
        KeyEvent.KEYCODE_BUTTON_THUMBR -> 10
        KeyEvent.KEYCODE_DPAD_UP -> 11
        KeyEvent.KEYCODE_DPAD_DOWN -> 12
        KeyEvent.KEYCODE_DPAD_LEFT -> 13
        KeyEvent.KEYCODE_DPAD_RIGHT -> 14
        // L2 / R2 are surfaced via analog triggers (lt / rt) below
        // rather than the button bitmask.
        else -> null
    }
}
