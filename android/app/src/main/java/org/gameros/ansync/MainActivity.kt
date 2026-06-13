package org.gameros.ansync

import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp

/**
 * Companion entry point.
 *
 * Step 7c scope is structural — the surface area below is a stub
 * that confirms the build produces a launchable APK. Real pairing
 * UX (QR scan / PIN entry / host discovery list) lands alongside
 * Step 7d when the QUIC client + MediaProjection capture come
 * online.
 */
class MainActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
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
    Column(
        modifier = Modifier
            .fillMaxSize()
            .padding(24.dp),
        verticalArrangement = Arrangement.Center,
        horizontalAlignment = Alignment.CenterHorizontally,
    ) {
        Text(text = "ansync companion", style = MaterialTheme.typography.headlineMedium)
        Text(
            text = "Step 7c scaffold — pairing & capture UX wired in Step 7d.",
            style = MaterialTheme.typography.bodyMedium,
        )
    }
}
