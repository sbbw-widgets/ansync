package org.gameros.ansync

import android.content.Context
import android.content.Intent
import android.net.Uri
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.result.contract.ActivityResultContracts

/**
 * Translucent shim that pops the SAF tree picker so the user can
 * select (or re-select) the folder `AnsyncFsServer` will share with
 * the host. Persists the URI + read/write grant in
 * `SharedPreferences` keyed by [PREF_TREE_URI].
 *
 * Used when:
 *   * the QSTile / notification path catches the user toggling
 *     mirror but no tree is set, or
 *   * the host sends `Device.Mount` against a peer that doesn't
 *     have one yet (the service launches us to ask).
 */
class GrantStorageActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        val launcher = registerForActivityResult(
            ActivityResultContracts.OpenDocumentTree()
        ) { uri: Uri? ->
            if (uri != null) {
                contentResolver.takePersistableUriPermission(
                    uri,
                    Intent.FLAG_GRANT_READ_URI_PERMISSION or Intent.FLAG_GRANT_WRITE_URI_PERMISSION,
                )
                getSharedPreferences(PREFS, Context.MODE_PRIVATE)
                    .edit()
                    .putString(PREF_TREE_URI, uri.toString())
                    .apply()
                val svc = Intent(this, AnsyncCompanionService::class.java).apply {
                    action = AnsyncCompanionService.ACTION_TREE_URI_UPDATED
                }
                startService(svc)
            }
            finish()
        }
        launcher.launch(null)
    }
}
