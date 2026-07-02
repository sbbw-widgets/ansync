package org.gameros.ansync.input

import android.content.Context
import android.text.InputType
import android.view.KeyEvent
import android.view.View
import android.view.inputmethod.EditorInfo
import android.view.inputmethod.InputConnection
import android.view.inputmethod.InputConnectionWrapper
import android.view.inputmethod.InputMethodManager
import android.widget.EditText
import androidx.compose.foundation.layout.size
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import androidx.compose.ui.viewinterop.AndroidView
import org.gameros.ansync.KeycodeMap

/**
 * Offscreen [EditText] whose [InputConnection] is hijacked so that
 * every IME event — committed text, surrounding-text deletes, raw
 * key events — is forwarded straight to the host as a sequence of
 * `KeyPress` events without ever mutating the local text buffer.
 *
 * The buffer-less design is deliberate: sharing a text buffer with
 * the IME would let autocorrect / predictive replacement rewrite the
 * composition and surface those rewrites as misleading length deltas,
 * translating into spurious backspaces that wipe the host's field.
 */
internal class HostKeyboardEditText(ctx: Context) : EditText(ctx) {
    init {
        inputType = InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_FLAG_NO_SUGGESTIONS
        setBackgroundResource(0)
        setTextColor(0)
        isFocusable = true
        isFocusableInTouchMode = true
        importantForAutofill = View.IMPORTANT_FOR_AUTOFILL_NO
    }

    override fun onCreateInputConnection(outAttrs: EditorInfo): InputConnection {
        outAttrs.imeOptions = EditorInfo.IME_FLAG_NO_EXTRACT_UI or
            EditorInfo.IME_FLAG_NO_FULLSCREEN or
            EditorInfo.IME_FLAG_NO_PERSONALIZED_LEARNING
        outAttrs.inputType = InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_FLAG_NO_SUGGESTIONS
        val base = super.onCreateInputConnection(outAttrs)
        return HostKeyboardInputConnection(base, true)
    }
}

private class HostKeyboardInputConnection(
    base: InputConnection,
    mutable: Boolean,
) : InputConnectionWrapper(base, mutable) {

    override fun commitText(text: CharSequence?, newCursorPosition: Int): Boolean {
        text?.forEach { sendCharAsKey(it) }
        return true
    }

    override fun setComposingText(text: CharSequence?, newCursorPosition: Int): Boolean = true
    override fun finishComposingText(): Boolean = true
    override fun setComposingRegion(start: Int, end: Int): Boolean = true

    override fun deleteSurroundingText(beforeLength: Int, afterLength: Int): Boolean {
        repeat(beforeLength) {
            sendKeyToHost(14, true); sendKeyToHost(14, false)
        }
        repeat(afterLength) {
            sendKeyToHost(111, true); sendKeyToHost(111, false)
        }
        return true
    }

    override fun deleteSurroundingTextInCodePoints(beforeLength: Int, afterLength: Int): Boolean =
        deleteSurroundingText(beforeLength, afterLength)

    override fun sendKeyEvent(event: KeyEvent): Boolean {
        if (event.action == KeyEvent.ACTION_DOWN || event.action == KeyEvent.ACTION_UP) {
            KeycodeMap.toEvdev(event.keyCode)?.let { evdev ->
                sendKeyToHost(evdev, event.action == KeyEvent.ACTION_DOWN)
                return true
            }
        }
        return super.sendKeyEvent(event)
    }

    override fun performEditorAction(editorAction: Int): Boolean {
        sendKeyToHost(28, true); sendKeyToHost(28, false)
        return true
    }
}

/**
 * Translate a Unicode `Char` into one or more evdev key presses.
 * ASCII letters + digits + shifted punctuation synthesise a left-shift
 * held around the base key; non-ASCII glyphs drop (the wire only
 * carries evdev keycodes — use the clipboard path for non-ASCII).
 */
private fun sendCharAsKey(c: Char) {
    val (evdev, shifted) = when (c) {
        '\n' -> 28 to false
        '\t' -> 15 to false
        ' ' -> 57 to false
        in 'a'..'z' -> KeycodeMap.toEvdev(KeyEvent.KEYCODE_A + (c - 'a'))!! to false
        in 'A'..'Z' -> KeycodeMap.toEvdev(KeyEvent.KEYCODE_A + (c - 'A'))!! to true
        in '0'..'9' -> KeycodeMap.toEvdev(KeyEvent.KEYCODE_0 + (c - '0'))!! to false
        '-' -> 12 to false
        '_' -> 12 to true
        '=' -> 13 to false
        '+' -> 13 to true
        '[' -> 26 to false
        '{' -> 26 to true
        ']' -> 27 to false
        '}' -> 27 to true
        '\\' -> 43 to false
        '|' -> 43 to true
        ';' -> 39 to false
        ':' -> 39 to true
        '\'' -> 40 to false
        '"' -> 40 to true
        '`' -> 41 to false
        '~' -> 41 to true
        ',' -> 51 to false
        '<' -> 51 to true
        '.' -> 52 to false
        '>' -> 52 to true
        '/' -> 53 to false
        '?' -> 53 to true
        '!' -> 2 to true
        '@' -> 3 to true
        '#' -> 4 to true
        '$' -> 5 to true
        '%' -> 6 to true
        '^' -> 7 to true
        '&' -> 8 to true
        '*' -> 9 to true
        '(' -> 10 to true
        ')' -> 11 to true
        else -> return
    }
    if (shifted) {
        sendKeyToHost(42, true)
        sendKeyToHost(evdev, true)
        sendKeyToHost(evdev, false)
        sendKeyToHost(42, false)
    } else {
        sendKeyToHost(evdev, true)
        sendKeyToHost(evdev, false)
    }
}

/**
 * Off-screen EditText + IME show / hide side-effect. Mount this
 * composable once at scaffold scope; toggling [open] pops or
 * dismisses the phone's soft keyboard without navigating away from
 * whichever mode surface is currently active.
 *
 * The view is sized `1 dp × 1 dp` (not `0 × 0`) because Android
 * refuses focus in touch mode for views with zero dimensions, and
 * `requestFocus() == false` makes `showSoftInput` a silent no-op.
 * The `post` bounce off the view thread lets the AndroidView finish
 * attachment (window token in place) before the IMM call fires, so
 * the very first toggle from a fresh activity still shows the pane.
 */
@Composable
internal fun IMESink(open: Boolean) {
    var ref by remember { mutableStateOf<HostKeyboardEditText?>(null) }
    LaunchedEffect(open, ref) {
        val et = ref ?: return@LaunchedEffect
        et.post {
            val imm = et.context.getSystemService(Context.INPUT_METHOD_SERVICE)
                as InputMethodManager
            if (open) {
                et.requestFocus()
                imm.showSoftInput(et, InputMethodManager.SHOW_IMPLICIT)
            } else {
                imm.hideSoftInputFromWindow(et.windowToken, 0)
                et.clearFocus()
            }
        }
    }
    AndroidView(
        factory = { ctx -> HostKeyboardEditText(ctx).also { ref = it } },
        modifier = Modifier.size(1.dp),
    )
}
