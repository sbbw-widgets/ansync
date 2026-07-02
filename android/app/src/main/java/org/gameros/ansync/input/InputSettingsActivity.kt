package org.gameros.ansync.input

import android.content.Context
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Slider
import androidx.compose.material3.Text
import androidx.compose.material3.darkColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import com.composables.icons.lucide.Gamepad2
import com.composables.icons.lucide.Info
import com.composables.icons.lucide.Lucide
import org.gameros.ansync.PREFS
import org.gameros.ansync.ui.dialog.AnsyncDialog
import org.gameros.ansync.ui.dialog.DialogButton

/**
 * Translucent settings dialog for [InputActivity]. Content adapts to
 * the currently-active mode:
 *
 *  - Gamepad: per-button size + alpha sliders + Reset defaults +
 *    "Edit positions" toggle. Position editing lives inside the
 *    gamepad surface itself (long-press to grab) — this activity just
 *    flips the edit-mode pref and dismisses.
 *  - Touchpad / Keyboard: informational only for now; the surfaces
 *    don't yet expose configurable parameters (touchscreen-mode
 *    toggle stays in the header pill of the touchpad).
 */
class InputSettingsActivity : ComponentActivity() {

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        val mode = InputMode.fromWire(intent.getStringExtra(EXTRA_MODE)) ?: InputMode.Touchpad
        setContent {
            MaterialTheme(colorScheme = darkColorScheme()) {
                when (mode) {
                    InputMode.Gamepad -> GamepadConfigDialog(
                        ctx = this,
                        onDismiss = { finish() },
                    )
                    InputMode.Touchpad -> InformationalDialog(
                        title = "Touchpad settings",
                        message = "No configurable options yet. The touchpad ↔ " +
                            "touchscreen mode toggle lives on the header pill of " +
                            "the touchpad surface.",
                        onDismiss = { finish() },
                    )
                }
            }
        }
    }

    companion object {
        const val EXTRA_MODE = "mode"
    }
}

@Composable
private fun InformationalDialog(title: String, message: String, onDismiss: () -> Unit) {
    AnsyncDialog(
        icon = Lucide.Info,
        title = title,
        subtitle = message,
        onDismiss = onDismiss,
        actions = { DialogButton("OK") { onDismiss() } },
        body = {},
    )
}

@Composable
private fun GamepadConfigDialog(ctx: Context, onDismiss: () -> Unit) {
    var layout by remember { mutableStateOf(GamepadLayout.load(ctx)) }
    AnsyncDialog(
        icon = Lucide.Gamepad2,
        title = "Virtual gamepad layout",
        subtitle = "Tune the size and transparency of each on-screen control. " +
            "Enter edit mode to drag buttons around the surface.",
        onDismiss = onDismiss,
        actions = {
            DialogButton("Reset") {
                layout = GamepadLayout.DEFAULT
                layout.persist(ctx)
            }
            DialogButton("Edit positions") {
                setEditMode(ctx, true)
                onDismiss()
            }
            DialogButton("Done") { onDismiss() }
        },
    ) {
        Column(
            modifier = Modifier
                .heightWithMaxCap()
                .verticalScroll(rememberScrollState()),
            verticalArrangement = Arrangement.spacedBy(10.dp),
        ) {
            for (id in GamepadButton.entries) {
                val p = layout.buttons[id] ?: continue
                ButtonRow(
                    label = "${id.label}  (${id.name.lowercase()})",
                    radius = p.radius,
                    alpha = p.alpha,
                    minRadius = 16f,
                    maxRadius = 60f,
                    onChange = { r, a ->
                        val next = layout.withButton(id, p.copy(radius = r, alpha = a))
                        layout = next
                        next.persist(ctx)
                    },
                )
            }
            Spacer(Modifier.height(4.dp))
            for (side in GamepadStick.entries) {
                val p = layout.sticks[side] ?: continue
                StickRow(
                    label = "${if (side == GamepadStick.L) "Left" else "Right"} stick",
                    outer = p.outerRadius,
                    thumb = p.thumbRadius,
                    alpha = p.alpha,
                    onChange = { o, t, a ->
                        val next = layout.withStick(
                            side,
                            p.copy(outerRadius = o, thumbRadius = t, alpha = a),
                        )
                        layout = next
                        next.persist(ctx)
                    },
                )
            }
        }
    }
}

@Composable
private fun ButtonRow(
    label: String,
    radius: Float,
    alpha: Float,
    minRadius: Float,
    maxRadius: Float,
    onChange: (Float, Float) -> Unit,
) {
    Column(
        modifier = Modifier
            .fillMaxWidth()
            .background(
                color = MaterialTheme.colorScheme.surfaceVariant.copy(alpha = 0.3f),
                shape = RoundedCornerShape(12.dp),
            )
            .padding(horizontal = 12.dp, vertical = 8.dp),
        verticalArrangement = Arrangement.spacedBy(4.dp),
    ) {
        Text(label, color = MaterialTheme.colorScheme.onSurface, style = MaterialTheme.typography.labelLarge)
        LabelledSlider("Size", radius, minRadius, maxRadius) { r ->
            onChange(r, alpha)
        }
        LabelledSlider("Alpha", alpha, 0.15f, 1f) { a ->
            onChange(radius, a)
        }
    }
}

@Composable
private fun StickRow(
    label: String,
    outer: Float,
    thumb: Float,
    alpha: Float,
    onChange: (Float, Float, Float) -> Unit,
) {
    Column(
        modifier = Modifier
            .fillMaxWidth()
            .background(
                color = MaterialTheme.colorScheme.surfaceVariant.copy(alpha = 0.3f),
                shape = RoundedCornerShape(12.dp),
            )
            .padding(horizontal = 12.dp, vertical = 8.dp),
        verticalArrangement = Arrangement.spacedBy(4.dp),
    ) {
        Text(label, color = MaterialTheme.colorScheme.onSurface, style = MaterialTheme.typography.labelLarge)
        LabelledSlider("Base radius", outer, 50f, 140f) { o ->
            val cappedThumb = thumb.coerceAtMost(o - 8f)
            onChange(o, cappedThumb, alpha)
        }
        LabelledSlider("Thumb radius", thumb, 20f, 70f) { t ->
            onChange(outer, t.coerceAtMost(outer - 8f), alpha)
        }
        LabelledSlider("Alpha", alpha, 0.15f, 1f) { a ->
            onChange(outer, thumb, a)
        }
    }
}

@Composable
private fun LabelledSlider(
    label: String,
    value: Float,
    from: Float,
    to: Float,
    onValueChange: (Float) -> Unit,
) {
    Row(
        modifier = Modifier.fillMaxWidth(),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(8.dp),
    ) {
        Text(
            text = label,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
            style = MaterialTheme.typography.bodySmall,
            modifier = Modifier.padding(end = 4.dp),
        )
        Slider(
            value = value,
            onValueChange = onValueChange,
            valueRange = from..to,
            modifier = Modifier.weight(1f),
        )
        Text(
            text = "%.1f".format(value),
            color = MaterialTheme.colorScheme.onSurface,
            style = MaterialTheme.typography.bodySmall,
        )
    }
}

/// Cap the dialog body height so long lists scroll instead of pushing
/// the action row off screen. 480 dp is a comfortable ceiling on a
/// phone; tablets naturally center the card and 480 dp still leaves
/// the actions visible.
private fun Modifier.heightWithMaxCap(): Modifier = this.then(
    Modifier.heightIn(max = 480.dp)
)

// ── Edit-mode pref helpers (read by GamepadSurface.onResume) ─────────

const val PREF_GAMEPAD_EDIT_MODE = "gamepad_edit_mode"

internal fun editModeActive(ctx: Context): Boolean =
    ctx.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
        .getBoolean(PREF_GAMEPAD_EDIT_MODE, false)

internal fun setEditMode(ctx: Context, on: Boolean) {
    ctx.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
        .edit()
        .putBoolean(PREF_GAMEPAD_EDIT_MODE, on)
        .apply()
}
