package org.gameros.ansync.input

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.FloatingActionButton
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.unit.dp
import com.composables.icons.lucide.Gamepad2
import com.composables.icons.lucide.Keyboard
import com.composables.icons.lucide.Lucide
import com.composables.icons.lucide.Settings2
import com.composables.icons.lucide.SquareMousePointer

/**
 * Left-edge floating rail — mimics the strip of shortcut keys on a
 * Wacom / XP-Pen tablet. Four rounded-square FABs stacked vertically,
 * elevated on top of whichever surface is active.
 *
 *   Touchpad mode      — switches to [InputMode.Touchpad]
 *   Virtual gamepad    — switches to [InputMode.Gamepad]
 *   Soft keyboard      — TOGGLES the phone's on-screen IME (works from
 *                        both modes). Its "active" tint tracks the
 *                        current [imeOpen] state; text typed there
 *                        streams to the host as evdev keypresses.
 *   Config             — opens the settings popup for the active mode.
 *
 * The active mode's tile draws in the primary color; inactive tiles
 * fall to surface-variant so the current mode is unambiguous from any
 * viewing angle. Physical keyboard events forward through
 * [InputActivity.dispatchKeyEvent] regardless of the IME toggle — see
 * [KeyboardStatusPill] for the on-surface HW-kbd indicator.
 */
@Composable
fun InputRail(
    mode: InputMode,
    imeOpen: Boolean,
    onSelectMode: (InputMode) -> Unit,
    onToggleKeyboard: () -> Unit,
    onOpenSettings: () -> Unit,
    modifier: Modifier = Modifier,
) {
    Column(
        modifier = modifier.padding(PaddingValues(start = 12.dp, top = 12.dp, bottom = 12.dp)),
        verticalArrangement = Arrangement.spacedBy(10.dp),
    ) {
        RailButton(
            icon = Lucide.SquareMousePointer,
            active = mode == InputMode.Touchpad,
            contentDescription = "Touchpad mode",
            onClick = { onSelectMode(InputMode.Touchpad) },
        )
        RailButton(
            icon = Lucide.Gamepad2,
            active = mode == InputMode.Gamepad,
            contentDescription = "Gamepad mode",
            onClick = { onSelectMode(InputMode.Gamepad) },
        )
        RailButton(
            icon = Lucide.Keyboard,
            active = imeOpen,
            contentDescription = if (imeOpen) "Hide soft keyboard" else "Show soft keyboard",
            onClick = onToggleKeyboard,
        )
        RailButton(
            icon = Lucide.Settings2,
            active = false,
            contentDescription = "Configure input surface",
            onClick = onOpenSettings,
        )
    }
}

@Composable
private fun RailButton(
    icon: ImageVector,
    active: Boolean,
    contentDescription: String,
    onClick: () -> Unit,
) {
    val bg = if (active) MaterialTheme.colorScheme.primary
    else MaterialTheme.colorScheme.surface.copy(alpha = 0.85f)
    val fg = if (active) MaterialTheme.colorScheme.onPrimary
    else MaterialTheme.colorScheme.onSurface
    FloatingActionButton(
        onClick = onClick,
        modifier = Modifier.size(52.dp),
        containerColor = bg,
        contentColor = fg,
        shape = RoundedCornerShape(14.dp),
    ) {
        Icon(imageVector = icon, contentDescription = contentDescription, tint = fg)
    }
}
