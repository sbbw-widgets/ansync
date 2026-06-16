package org.gameros.ansync

import android.Manifest
import android.content.Context
import android.content.Intent
import android.os.Bundle
import android.provider.Settings
import android.util.Log
import android.widget.Toast
import androidx.activity.ComponentActivity
import androidx.activity.result.ActivityResultLauncher
import androidx.activity.result.contract.ActivityResultContracts

/**
 * Translucent shim that runs ONE [SetupStep] then refreshes the
 * setup notification + returns to the shade. Invoked by the user
 * tapping the persistent notif from [SetupNotif], or from the
 * launcher icon (which auto-redirects to the next pending step).
 *
 * We register every possible launcher up-front because
 * `ActivityResultContracts` must be wired before `onResume`.
 */
class SetupStepActivity : ComponentActivity() {

    private lateinit var requestPermLauncher: ActivityResultLauncher<String>
    private lateinit var settingsLauncher: ActivityResultLauncher<Intent>

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)

        requestPermLauncher = registerForActivityResult(
            ActivityResultContracts.RequestPermission()
        ) { afterStep() }

        settingsLauncher = registerForActivityResult(
            ActivityResultContracts.StartActivityForResult()
        ) { afterStep() }

        val requestedKey = intent.getStringExtra(EXTRA_STEP_KEY)
        val step = SetupStep.values().firstOrNull { it.key == requestedKey }
            ?: SetupStep.nextPending(this)
        if (step == null) {
            Toast.makeText(this, "ansync setup complete", Toast.LENGTH_SHORT).show()
            SetupNotif.refresh(this)
            finish()
            return
        }
        if (step.isDone(this)) {
            // User tapped a stale notif — fast-forward to the next pending.
            val next = SetupStep.nextPending(this)
            if (next == null) {
                Toast.makeText(this, "ansync setup complete", Toast.LENGTH_SHORT).show()
                SetupNotif.refresh(this)
                finish()
                return
            }
            startActivity(Intent(this, SetupStepActivity::class.java).apply {
                addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_CLEAR_TASK)
                putExtra(EXTRA_STEP_KEY, next.key)
            })
            finish()
            return
        }
        runStep(step)
    }

    private fun runStep(step: SetupStep) {
        Toast.makeText(this, step.body, Toast.LENGTH_LONG).show()
        when (step) {
            SetupStep.Notifications -> requestPermLauncher.launch(
                Manifest.permission.POST_NOTIFICATIONS
            )
            SetupStep.Microphone -> requestPermLauncher.launch(
                Manifest.permission.RECORD_AUDIO
            )
            SetupStep.Accessibility -> launchSettings(
                Intent(Settings.ACTION_ACCESSIBILITY_SETTINGS)
            )
            SetupStep.NotificationListener -> launchSettings(
                Intent(Settings.ACTION_NOTIFICATION_LISTENER_SETTINGS)
            )
            SetupStep.MiuiAutostart -> launchMiuiAutostart()
        }
    }

    private fun launchSettings(intent: Intent) {
        try {
            settingsLauncher.launch(intent)
        } catch (e: Exception) {
            Log.w(TAG, "settings launch failed: $e")
            afterStep()
        }
    }

    private fun launchMiuiAutostart() {
        val intent = Intent("miui.intent.action.OP_AUTO_START").apply {
            addCategory(Intent.CATEGORY_DEFAULT)
            putExtra("package_name", packageName)
            putExtra("package_label", "ansync companion")
        }
        try {
            settingsLauncher.launch(intent)
        } catch (e: Exception) {
            Log.w(TAG, "MIUI autostart intent failed: $e")
        }
        // Whether or not the user actually flips the toggle, we mark
        // the hint as shown so the wizard doesn't loop on it forever.
        getSharedPreferences(PREFS, Context.MODE_PRIVATE)
            .edit()
            .putBoolean(SetupStep.PREF_MIUI_AUTOSTART_DONE, true)
            .apply()
    }

    private fun afterStep() {
        SetupNotif.refresh(this)
        kickService(AnsyncCompanionService.ACTION_REFRESH_SETUP)
        // Chain into the next pending step in the same Activity session
        // so the user sees prompts back-to-back without having to swipe
        // the shade and tap the notif between each grant. Stops when
        // there's nothing left, which auto-cancels the notif via
        // SetupNotif.refresh above.
        val next = SetupStep.nextPending(this)
        if (next != null) {
            startActivity(Intent(this, SetupStepActivity::class.java).apply {
                addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_CLEAR_TASK)
                putExtra(EXTRA_STEP_KEY, next.key)
            })
        } else {
            Toast.makeText(this, "ansync setup complete", Toast.LENGTH_SHORT).show()
        }
        finish()
    }

    private fun kickService(action: String) {
        val svc = Intent(this, AnsyncCompanionService::class.java).setAction(action)
        try { startService(svc) } catch (_: Exception) { }
    }

    companion object {
        private const val TAG = "ansync.setup"
        const val EXTRA_STEP_KEY = "step_key"
    }
}
