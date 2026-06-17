package org.gameros.ansync

import android.app.Activity
import android.content.Context
import android.content.Intent
import android.media.projection.MediaProjectionConfig
import android.media.projection.MediaProjectionManager
import android.os.Build
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.result.contract.ActivityResultContracts

/**
 * Translucent shim that pops the system MediaProjection picker, then
 * forwards the result into `AnsyncCompanionService` so capture can
 * start. Visible only as the system dialog itself — no app UI.
 *
 * Triggered by either:
 *   * the `MirrorTile` QSTile when the user toggles it on, or
 *   * the service's "tap to grant" notification fired when the host
 *     issued `Device.ShowScreen` but the projection token is stale.
 *
 * MediaProjection consent is per-session and cannot be cached, so we
 * pop this every time a fresh capture starts.
 */
class GrantScreenCaptureActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        val launcher = registerForActivityResult(
            ActivityResultContracts.StartActivityForResult()
        ) { result ->
            if (result.resultCode == Activity.RESULT_OK && result.data != null) {
                val svc = Intent(this, AnsyncCompanionService::class.java).apply {
                    action = AnsyncCompanionService.ACTION_START_CAPTURE
                    putExtra(AnsyncCompanionService.EXTRA_RESULT_CODE, result.resultCode)
                    putExtra(AnsyncCompanionService.EXTRA_RESULT_DATA, result.data)
                }
                if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
                    startForegroundService(svc)
                } else {
                    startService(svc)
                }
            }
            finish()
        }
        val mgr = getSystemService(Context.MEDIA_PROJECTION_SERVICE) as MediaProjectionManager
        // Android 14+ ships `MediaProjectionConfig.createConfigForDefaultDisplay()`
        // which pre-selects the full default display in the SystemUI
        // chooser. The user still has to tap "Start now" — that's the
        // OS-mandated security gate, we can't bypass it — but the
        // intermediate "what do you want to share" step is skipped so
        // it feels like a single confirmation, not a two-step picker.
        // Older Android versions just see the standard intent.
        val intent = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
            mgr.createScreenCaptureIntent(
                MediaProjectionConfig.createConfigForDefaultDisplay()
            )
        } else {
            mgr.createScreenCaptureIntent()
        }
        launcher.launch(intent)
    }
}
