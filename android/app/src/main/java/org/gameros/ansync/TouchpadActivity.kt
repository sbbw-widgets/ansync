package org.gameros.ansync

import android.os.Bundle
import android.view.MotionEvent
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.input.pointer.pointerInteropFilter
import androidx.compose.ui.unit.dp
import androidx.compose.ui.ExperimentalComposeUiApi

/**
 * Full-screen touchpad: every `MotionEvent` is mapped to a
 * relative-movement `InputMessage::MouseMove` and pushed to the host
 * via `NativeBridge.nativeSendInputMessage`. Tap → MouseButton{1}
 * press + release. Two-finger swipe → MouseWheel (Step 9.5e+ stretch
 * goal; for the MVP only single-finger drag + tap are wired).
 *
 * Held-finger semantics: the first event of a stroke is a "press"
 * (cursor anchor); subsequent events emit MouseMove deltas; the last
 * (ACTION_UP) sends MouseButton{1,false}.
 */
class TouchpadActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContent {
            MaterialTheme {
                TouchpadCanvas()
            }
        }
    }
}

@OptIn(ExperimentalComposeUiApi::class)
@Composable
private fun TouchpadCanvas() {
    var last by remember { mutableStateOf<Pair<Float, Float>?>(null) }
    var status by remember { mutableStateOf("touchpad ready") }

    LaunchedEffect(Unit) { /* surface settle */ }

    Box(
        modifier = Modifier
            .fillMaxSize()
            .background(Color(0xFF101418))
            .pointerInteropFilter { event ->
                when (event.actionMasked) {
                    MotionEvent.ACTION_DOWN -> {
                        last = event.x to event.y
                        val ok = NativeBridge.nativeSendInputMessage(
                            WireInputMessage.MouseButton(button = 1, pressed = true).encode()
                        )
                        status = if (ok) "tap-down" else "send failed"
                    }
                    MotionEvent.ACTION_MOVE -> {
                        val (lx, ly) = last ?: (event.x to event.y)
                        val dx = (event.x - lx).toInt()
                        val dy = (event.y - ly).toInt()
                        if (dx != 0 || dy != 0) {
                            NativeBridge.nativeSendInputMessage(
                                WireInputMessage.MouseMove(dx = dx, dy = dy).encode()
                            )
                            last = event.x to event.y
                        }
                    }
                    MotionEvent.ACTION_UP, MotionEvent.ACTION_CANCEL -> {
                        NativeBridge.nativeSendInputMessage(
                            WireInputMessage.MouseButton(button = 1, pressed = false).encode()
                        )
                        last = null
                        status = "tap-up"
                    }
                }
                true
            }
    ) {
        Text(
            text = "ansync touchpad — drag to move cursor; tap to click\n$status",
            color = Color.White,
            modifier = Modifier
                .align(Alignment.TopStart)
                .padding(24.dp),
            style = MaterialTheme.typography.bodyMedium,
        )
    }
}
