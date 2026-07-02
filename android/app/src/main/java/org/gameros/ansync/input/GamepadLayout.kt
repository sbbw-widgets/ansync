package org.gameros.ansync.input

import android.content.Context
import org.gameros.ansync.PREFS
import org.json.JSONObject

/**
 * Buttons the virtual gamepad renders. Wire bit column mirrors
 * `ansync_input::uinput::Gamepad::GP_BTN_LIST` on the host — keep in
 * sync when either side changes.
 *
 * [L2]/[R2] have no bit slot because the wire protocol carries them
 * as analog `lt`/`rt` bytes (0..255). The virtual UI still exposes
 * them as discrete tap targets: touching = 255, releasing = 0. Real
 * physical triggers on a Bluetooth pad passing through
 * [InputActivity.dispatchGenericMotionEvent] still emit the proper
 * pressure ramp.
 */
enum class GamepadButton(val bit: Int?, val label: String) {
    A(0, "A"),
    B(1, "B"),
    Y(2, "Y"),
    X(3, "X"),
    L1(4, "L1"),
    R1(5, "R1"),
    Select(6, "SEL"),
    Start(7, "START"),
    Mode(8, "H"),
    ThumbL(9, "L3"),
    ThumbR(10, "R3"),
    L2(null, "L2"),
    R2(null, "R2"),
    DpadUp(11, "▲"),
    DpadDown(12, "▼"),
    DpadLeft(13, "◀"),
    DpadRight(14, "▶"),
}

enum class GamepadStick { L, R }

/**
 * Placement of a single tap-button on the virtual gamepad canvas.
 *
 * Coords are FRACTIONAL (0..1) of the canvas — orientation-agnostic
 * so the layout survives rotation. Radius + alpha are absolute dp /
 * unit values the renderer converts at draw time.
 */
data class ButtonPlacement(
    val cx: Float,
    val cy: Float,
    val radius: Float,
    val alpha: Float,
)

/**
 * Placement of an analog stick. The user drags inside [outerRadius]
 * of the base to deflect the thumb; the delta becomes the `lx/ly`
 * (or `rx/ry`) axis reading.
 */
data class StickPlacement(
    val cx: Float,
    val cy: Float,
    val outerRadius: Float,
    val thumbRadius: Float,
    val alpha: Float,
)

data class GamepadLayout(
    val buttons: Map<GamepadButton, ButtonPlacement>,
    val sticks: Map<GamepadStick, StickPlacement>,
) {
    fun withButton(id: GamepadButton, p: ButtonPlacement): GamepadLayout =
        copy(buttons = buttons.toMutableMap().apply { put(id, p) })

    fun withStick(id: GamepadStick, p: StickPlacement): GamepadLayout =
        copy(sticks = sticks.toMutableMap().apply { put(id, p) })

    fun toJson(): String {
        val root = JSONObject()
        val btns = JSONObject()
        for ((id, p) in buttons) {
            btns.put(
                id.name,
                JSONObject()
                    .put("cx", p.cx.toDouble())
                    .put("cy", p.cy.toDouble())
                    .put("r", p.radius.toDouble())
                    .put("a", p.alpha.toDouble()),
            )
        }
        val stx = JSONObject()
        for ((id, p) in sticks) {
            stx.put(
                id.name,
                JSONObject()
                    .put("cx", p.cx.toDouble())
                    .put("cy", p.cy.toDouble())
                    .put("or", p.outerRadius.toDouble())
                    .put("tr", p.thumbRadius.toDouble())
                    .put("a", p.alpha.toDouble()),
            )
        }
        root.put("buttons", btns)
        root.put("sticks", stx)
        return root.toString()
    }

    fun persist(ctx: Context) {
        ctx.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
            .edit()
            .putString(PREF_GAMEPAD_LAYOUT, toJson())
            .apply()
    }

    companion object {
        /**
         * Default layout tuned for a phone held in landscape. Face
         * cluster on the right, sticks bottom-outer, shoulders +
         * triggers top-outer, meta row (Select / Home / Start) along
         * the bottom center.
         */
        val DEFAULT: GamepadLayout = run {
            val face = { cx: Float, cy: Float -> ButtonPlacement(cx, cy, 36f, 0.85f) }
            val meta = { cx: Float, cy: Float -> ButtonPlacement(cx, cy, 26f, 0.7f) }
            val shoulder = { cx: Float, cy: Float -> ButtonPlacement(cx, cy, 34f, 0.85f) }
            val stickClick = { cx: Float, cy: Float -> ButtonPlacement(cx, cy, 22f, 0.7f) }
            val dpad = { cx: Float, cy: Float -> ButtonPlacement(cx, cy, 30f, 0.85f) }
            val stick = { cx: Float, cy: Float ->
                StickPlacement(cx, cy, outerRadius = 90f, thumbRadius = 42f, alpha = 0.85f)
            }
            GamepadLayout(
                buttons = mapOf(
                    GamepadButton.L2 to shoulder(0.075f, 0.12f),
                    GamepadButton.L1 to shoulder(0.16f, 0.12f),
                    GamepadButton.R1 to shoulder(0.84f, 0.12f),
                    GamepadButton.R2 to shoulder(0.925f, 0.12f),
                    GamepadButton.Y to face(0.87f, 0.32f),
                    GamepadButton.X to face(0.80f, 0.42f),
                    GamepadButton.B to face(0.94f, 0.42f),
                    GamepadButton.A to face(0.87f, 0.52f),
                    GamepadButton.Select to meta(0.42f, 0.90f),
                    GamepadButton.Mode to meta(0.50f, 0.92f),
                    GamepadButton.Start to meta(0.58f, 0.90f),
                    GamepadButton.ThumbL to stickClick(0.31f, 0.87f),
                    GamepadButton.ThumbR to stickClick(0.69f, 0.87f),
                    // DPAD cross to the right of the left stick,
                    // mirroring the face-button cluster on the far side.
                    GamepadButton.DpadUp to dpad(0.14f, 0.32f),
                    GamepadButton.DpadDown to dpad(0.14f, 0.52f),
                    GamepadButton.DpadLeft to dpad(0.07f, 0.42f),
                    GamepadButton.DpadRight to dpad(0.21f, 0.42f),
                ),
                sticks = mapOf(
                    GamepadStick.L to stick(0.14f, 0.68f),
                    GamepadStick.R to stick(0.86f, 0.68f),
                ),
            )
        }

        fun fromJson(s: String): GamepadLayout? = try {
            val root = JSONObject(s)
            val bJson = root.optJSONObject("buttons")
            val sJson = root.optJSONObject("sticks")
            val bs = mutableMapOf<GamepadButton, ButtonPlacement>()
            val sts = mutableMapOf<GamepadStick, StickPlacement>()
            if (bJson != null) {
                for (k in bJson.keys()) {
                    val id = GamepadButton.entries.firstOrNull { it.name == k } ?: continue
                    val o = bJson.getJSONObject(k)
                    bs[id] = ButtonPlacement(
                        cx = o.optDouble("cx", 0.5).toFloat(),
                        cy = o.optDouble("cy", 0.5).toFloat(),
                        radius = o.optDouble("r", 32.0).toFloat(),
                        alpha = o.optDouble("a", 0.8).toFloat().coerceIn(0.1f, 1f),
                    )
                }
            }
            if (sJson != null) {
                for (k in sJson.keys()) {
                    val id = GamepadStick.entries.firstOrNull { it.name == k } ?: continue
                    val o = sJson.getJSONObject(k)
                    sts[id] = StickPlacement(
                        cx = o.optDouble("cx", 0.5).toFloat(),
                        cy = o.optDouble("cy", 0.5).toFloat(),
                        outerRadius = o.optDouble("or", 80.0).toFloat(),
                        thumbRadius = o.optDouble("tr", 36.0).toFloat(),
                        alpha = o.optDouble("a", 0.8).toFloat().coerceIn(0.1f, 1f),
                    )
                }
            }
            // Fill any missing entries with defaults so a legacy JSON
            // that predates a new button doesn't leave the UI blank.
            val filledButtons = DEFAULT.buttons.toMutableMap()
            filledButtons.putAll(bs)
            val filledSticks = DEFAULT.sticks.toMutableMap()
            filledSticks.putAll(sts)
            GamepadLayout(filledButtons, filledSticks)
        } catch (_: Throwable) {
            null
        }

        fun load(ctx: Context): GamepadLayout {
            val raw = ctx.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
                .getString(PREF_GAMEPAD_LAYOUT, null)
            return raw?.let { fromJson(it) } ?: DEFAULT
        }
    }
}
