package org.gameros.ansync

import android.content.Intent
import android.hardware.camera2.CameraCharacteristics
import android.hardware.camera2.CameraManager
import android.media.MediaCodec
import android.os.Bundle
import android.util.Size
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Settings
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Slider
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.material3.darkColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.unit.dp
import org.gameros.ansync.ui.dialog.AnsyncDialog
import org.gameros.ansync.ui.dialog.DialogButton

/**
 * Translucent activity that hosts the Ansync-styled settings popup
 * for [tile.CameraTile]. Reachable via:
 *
 *   - Long-press on the tile → Android fires the
 *     `QS_TILE_PREFERENCES` action (declared on this activity's
 *     intent-filter).
 *   - First-time short tap when no config has been saved yet.
 *
 * Save persists to SharedPreferences and immediately spawns the
 * capture via `AnsyncCompanionService.ACTION_START_CAMERA`. Cancel
 * dismisses without changes.
 */
class CameraSettingsActivity : ComponentActivity() {

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        val startAfterSave = intent.getBooleanExtra(EXTRA_START_AFTER_SAVE, false)
        val initial = CameraLocalConfig.load(this)
        val cameras = collectCameras()
        val resolutions = collectResolutions(initial.cameraId)

        setContent {
            MaterialTheme(colorScheme = darkColorScheme()) {
                var camId by remember { mutableStateOf(initial.cameraId) }
                var w by remember { mutableStateOf(initial.width) }
                var h by remember { mutableStateOf(initial.height) }
                var fps by remember { mutableStateOf(initial.fps) }
                var bitrate by remember { mutableStateOf(initial.bitrateKbps) }
                var codec by remember { mutableStateOf(initial.codec) }
                var aspect by remember { mutableStateOf(initial.aspect) }
                var stab by remember { mutableStateOf(initial.stabilization) }

                AnsyncDialog(
                    icon = Icons.Filled.Settings,
                    title = "Camera share",
                    subtitle = "Pick the encoding the phone will send to the paired PC.",
                    onDismiss = { finish() },
                    actions = {
                        DialogButton("Cancel") { finish() }
                        DialogButton("Save") {
                            val cfg = CameraLocalConfig(
                                cameraId = camId,
                                width = w,
                                height = h,
                                fps = fps,
                                bitrateKbps = bitrate,
                                codec = codec,
                                aspect = aspect,
                                stabilization = stab,
                            )
                            cfg.persist(this@CameraSettingsActivity)
                            if (startAfterSave) {
                                val start = Intent(this@CameraSettingsActivity, AnsyncCompanionService::class.java)
                                    .setAction(AnsyncCompanionService.ACTION_START_CAMERA)
                                startService(start)
                            }
                            finish()
                        }
                    },
                ) {
                    Column(
                        modifier = Modifier
                            .verticalScroll(rememberScrollState()),
                        verticalArrangement = Arrangement.spacedBy(12.dp),
                    ) {
                        Picker(
                            label = "Camera",
                            selected = camId,
                            options = cameras.ifEmpty { listOf("0") },
                            onSelected = {
                                camId = it
                                // Re-derive resolutions for the newly-selected sensor.
                                val list = collectResolutions(it)
                                val closest = list.minByOrNull {
                                    kotlin.math.abs(it.width - w) + kotlin.math.abs(it.height - h)
                                }
                                if (closest != null) {
                                    w = closest.width
                                    h = closest.height
                                }
                            },
                            display = { formatCameraId(it) },
                        )
                        Picker(
                            label = "Resolution",
                            selected = Size(w, h),
                            options = resolutions,
                            onSelected = { w = it.width; h = it.height },
                            display = { "${it.width} × ${it.height}" },
                        )
                        Picker(
                            label = "FPS",
                            selected = fps,
                            options = listOf(24, 30, 60),
                            onSelected = { fps = it },
                            display = { "$it fps" },
                        )
                        Picker(
                            label = "Codec",
                            selected = codec,
                            options = CameraLocalConfig.Codec.entries,
                            onSelected = { codec = it },
                            display = { it.name },
                        )
                        Picker(
                            label = "Aspect",
                            selected = aspect,
                            options = CameraLocalConfig.Aspect.entries,
                            onSelected = { aspect = it },
                            display = { it.name },
                        )
                        Row(
                            modifier = Modifier.fillMaxWidth(),
                            verticalAlignment = Alignment.CenterVertically,
                            horizontalArrangement = Arrangement.SpaceBetween,
                        ) {
                            Text("Stabilization", color = MaterialTheme.colorScheme.onSurface)
                            Switch(checked = stab, onCheckedChange = { stab = it })
                        }
                        Column {
                            Text(
                                "Bitrate: ${bitrate / 1000} Mbps",
                                color = MaterialTheme.colorScheme.onSurface,
                            )
                            Slider(
                                value = bitrate.toFloat(),
                                onValueChange = { bitrate = it.toInt().coerceIn(500, 20_000) },
                                valueRange = 500f..20_000f,
                            )
                        }
                        Spacer(Modifier.height(4.dp))
                    }
                }
            }
        }
    }

    private fun collectCameras(): List<String> {
        val mgr = getSystemService(CAMERA_SERVICE) as? CameraManager ?: return listOf("0")
        return try {
            mgr.cameraIdList.toList().ifEmpty { listOf("0") }
        } catch (_: Exception) {
            listOf("0")
        }
    }

    private fun collectResolutions(cameraId: String): List<Size> {
        val mgr = getSystemService(CAMERA_SERVICE) as? CameraManager ?: return DEFAULT_RES
        return try {
            val chars = mgr.getCameraCharacteristics(cameraId)
            val map = chars.get(CameraCharacteristics.SCALER_STREAM_CONFIGURATION_MAP)
                ?: return DEFAULT_RES
            val sizes = map.getOutputSizes(MediaCodec::class.java)?.toList().orEmpty()
            val curated = sizes.filter { it.height in 480..2160 }
                .distinctBy { it.width * 10_000 + it.height }
                .sortedByDescending { it.width * it.height }
            curated.ifEmpty { DEFAULT_RES }
        } catch (_: Exception) {
            DEFAULT_RES
        }
    }

    private fun formatCameraId(id: String): String = when (id) {
        "0" -> "Back (0)"
        "1" -> "Front (1)"
        else -> "Camera $id"
    }

    companion object {
        const val EXTRA_START_AFTER_SAVE = "start_after_save"
        private val DEFAULT_RES = listOf(
            Size(1920, 1080),
            Size(1280, 720),
            Size(854, 480),
        )
    }
}

@Composable
private fun <T> Picker(
    label: String,
    selected: T,
    options: List<T>,
    onSelected: (T) -> Unit,
    display: (T) -> String,
) {
    var open by remember { mutableStateOf(false) }
    Row(
        modifier = Modifier
            .fillMaxWidth()
            .background(
                color = MaterialTheme.colorScheme.surfaceVariant.copy(alpha = 0.35f),
                shape = RoundedCornerShape(12.dp),
            )
            .clickable { open = true }
            .padding(horizontal = 16.dp, vertical = 12.dp),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.SpaceBetween,
    ) {
        Column {
            Text(
                text = label,
                style = MaterialTheme.typography.labelSmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
            Text(
                text = display(selected),
                style = MaterialTheme.typography.bodyLarge,
                color = MaterialTheme.colorScheme.onSurface,
            )
        }
        Text("▾", color = Color.White)
        DropdownMenu(expanded = open, onDismissRequest = { open = false }) {
            options.forEach { opt ->
                DropdownMenuItem(
                    text = { Text(display(opt)) },
                    onClick = {
                        onSelected(opt)
                        open = false
                    },
                )
            }
        }
    }
}
