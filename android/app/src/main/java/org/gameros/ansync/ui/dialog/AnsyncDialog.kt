package org.gameros.ansync.ui.dialog

import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.layout.wrapContentHeight
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.platform.LocalConfiguration
import androidx.compose.ui.unit.dp

/**
 * Responsive Ansync dialog surface. On portrait phone it's rendered
 * as a bottom sheet (like the "Buscando dispositivos cercanos"
 * reference); on tablet or landscape it's a centered floating card
 * (like the "Cast screen to device" reference).
 *
 * Same content [body] renders in both; layout adapts to the
 * screen's width. Wrap the whole thing in a translucent Activity
 * (`Theme.Ansync.Translucent`) that sets a scrim + click-outside-to-
 * dismiss.
 */
@Composable
fun AnsyncDialog(
    icon: ImageVector,
    title: String,
    subtitle: String? = null,
    onDismiss: () -> Unit,
    actions: @Composable () -> Unit,
    body: @Composable () -> Unit,
) {
    val cfg = LocalConfiguration.current
    val isWide = cfg.smallestScreenWidthDp >= 600 ||
        cfg.screenWidthDp > cfg.screenHeightDp
    val scrim = Color(0x99000000)
    Box(
        Modifier
            .fillMaxSize()
            .background(scrim)
            .clickable(enabled = true) { onDismiss() },
    ) {
        val alignment = if (isWide) Alignment.Center else Alignment.BottomCenter
        val shape = if (isWide) {
            RoundedCornerShape(28.dp)
        } else {
            RoundedCornerShape(topStart = 28.dp, topEnd = 28.dp)
        }
        val cardModifier = if (isWide) {
            Modifier
                .widthIn(min = 320.dp, max = 500.dp)
                .wrapContentHeight()
        } else {
            Modifier
                .fillMaxWidth()
                .heightIn(min = 220.dp)
        }
        Surface(
            modifier = Modifier
                .align(alignment)
                .padding(if (isWide) 24.dp else 0.dp)
                .then(cardModifier)
                .clickable(enabled = false) {},
            shape = shape,
            tonalElevation = 8.dp,
            color = MaterialTheme.colorScheme.surface,
        ) {
            Column(
                modifier = Modifier.padding(horizontal = 24.dp, vertical = 20.dp),
                verticalArrangement = Arrangement.spacedBy(12.dp),
            ) {
                Row(
                    verticalAlignment = Alignment.CenterVertically,
                    horizontalArrangement = Arrangement.spacedBy(12.dp),
                ) {
                    Icon(
                        imageVector = icon,
                        contentDescription = null,
                        tint = MaterialTheme.colorScheme.primary,
                    )
                    Text(
                        text = title,
                        style = MaterialTheme.typography.titleMedium,
                        color = MaterialTheme.colorScheme.onSurface,
                    )
                }
                if (subtitle != null) {
                    Text(
                        text = subtitle,
                        style = MaterialTheme.typography.bodyMedium,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                }
                body()
                Spacer(Modifier.width(4.dp))
                Row(
                    modifier = Modifier.fillMaxWidth(),
                    horizontalArrangement = Arrangement.End,
                ) {
                    actions()
                }
            }
        }
    }
}

@Composable
fun DialogButton(text: String, onClick: () -> Unit) {
    TextButton(onClick = onClick) { Text(text) }
}
