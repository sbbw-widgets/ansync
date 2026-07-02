package org.gameros.ansync.input

import androidx.activity.ComponentActivity
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.remember
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalConfiguration
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.unit.dp
import com.composables.icons.lucide.Keyboard
import com.composables.icons.lucide.KeyboardOff
import com.composables.icons.lucide.Lucide

/**
 * Compact floating badge advertising whether a hardware keyboard is
 * attached to the phone. Shown on top of both the touchpad and gamepad
 * surfaces because HW-key events forward to the host regardless of
 * which mode the rail selected.
 *
 * The pill re-reads [ComponentActivity.hasHardwareKeyboard] every time
 * Compose re-composes for a config change (the manifest declares
 * `configChanges="keyboardHidden"` on [InputActivity], so plug/unplug
 * lands here without an activity restart).
 */
@Composable
fun KeyboardStatusPill(modifier: Modifier = Modifier) {
    val ctx = LocalContext.current as? ComponentActivity
    val cfg = LocalConfiguration.current
    // Reading LocalConfiguration.current makes the composition
    // sensitive to config changes; re-derive under a key so kbd
    // plug / unplug re-flips the pill without an activity restart.
    val hasHw = remember(cfg) { ctx?.hasHardwareKeyboard() ?: false }
    Surface(
        modifier = modifier,
        shape = RoundedCornerShape(20.dp),
        color = MaterialTheme.colorScheme.surface.copy(alpha = 0.85f),
        tonalElevation = 3.dp,
    ) {
        Row(
            modifier = Modifier.padding(horizontal = 12.dp, vertical = 6.dp),
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(8.dp),
        ) {
            Icon(
                imageVector = if (hasHw) Lucide.Keyboard else Lucide.KeyboardOff,
                contentDescription = null,
                tint = if (hasHw) MaterialTheme.colorScheme.primary
                else MaterialTheme.colorScheme.onSurfaceVariant,
                modifier = Modifier.size(16.dp),
            )
            Text(
                text = if (hasHw) "HW keyboard → PC" else "No keyboard attached",
                color = MaterialTheme.colorScheme.onSurface,
                style = MaterialTheme.typography.labelSmall,
            )
        }
    }
}
