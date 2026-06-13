package org.gameros.ansync

import android.app.Activity
import android.content.Context
import android.content.Intent
import android.media.projection.MediaProjectionManager
import android.os.Build
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.compose.setContent
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.Button
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.unit.dp

/**
 * Companion entry point. Step 7d-3 renders:
 *   - This device's pubkey fingerprint (shared by NativeBridge).
 *   - A button that requests `MediaProjection` and starts the
 *     foreground capture service. The pairing UX (host discovery +
 *     fingerprint accept) lands in 7d-4.
 */
class MainActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)

        // Identity is created on the native side. Init here so the
        // activity can read the pubkey for the status screen even
        // when the foreground service has not yet started.
        NativeBridge.nativeInit(filesDir.absolutePath)

        setContent {
            MaterialTheme {
                Surface(modifier = Modifier.fillMaxSize()) {
                    StatusScreen()
                }
            }
        }
    }
}

@Composable
private fun StatusScreen() {
    val ctx = LocalContext.current
    var pubkey by remember { mutableStateOf<String?>(null) }
    var status by remember { mutableStateOf("idle") }

    LaunchedEffect(Unit) {
        pubkey = NativeBridge.nativeOurPubkeyHex()
    }

    val captureLauncher = rememberLauncherForActivityResult(
        ActivityResultContracts.StartActivityForResult()
    ) { result ->
        val data = result.data
        if (result.resultCode == Activity.RESULT_OK && data != null) {
            val svc = Intent(ctx, AnsyncCompanionService::class.java).apply {
                action = AnsyncCompanionService.ACTION_START_CAPTURE
                putExtra(AnsyncCompanionService.EXTRA_RESULT_CODE, result.resultCode)
                putExtra(AnsyncCompanionService.EXTRA_RESULT_DATA, data)
            }
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
                ctx.startForegroundService(svc)
            } else {
                ctx.startService(svc)
            }
            status = "capture started"
        } else {
            status = "permission denied"
        }
    }

    Column(
        modifier = Modifier
            .fillMaxSize()
            .padding(24.dp),
        verticalArrangement = Arrangement.Center,
        horizontalAlignment = Alignment.CenterHorizontally,
    ) {
        Text(text = "ansync companion", style = MaterialTheme.typography.headlineMedium)
        Spacer(modifier = Modifier.height(12.dp))
        Text(
            text = pubkey?.let { "pubkey: ${it.take(8)}…${it.takeLast(8)}" } ?: "loading identity…",
            style = MaterialTheme.typography.bodySmall,
        )
        Spacer(modifier = Modifier.height(24.dp))
        Button(onClick = {
            val mgr = ctx.getSystemService(Context.MEDIA_PROJECTION_SERVICE) as MediaProjectionManager
            captureLauncher.launch(mgr.createScreenCaptureIntent())
        }) {
            Text("Start screen capture")
        }
        Spacer(modifier = Modifier.height(12.dp))
        Text(text = "status: $status", style = MaterialTheme.typography.bodyMedium)
    }
}
