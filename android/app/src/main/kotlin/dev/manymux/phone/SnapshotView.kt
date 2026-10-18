package dev.manymux.phone

import android.content.Context
import android.graphics.Canvas
import android.view.View
import uniffi.manymux_android.Preview
import uniffi.manymux_android.Run

/**
 * One session's screen, drawn small.
 *
 * The same rows the terminal draws, through the same palette, at whatever size
 * makes the session's grid fit this square. It is not a picture of the session
 * so much as the session at a distance: at this size nobody is reading the
 * text, they are recognising the shape of it, which is what tells a build from
 * a shell from an editor at a glance and is the whole reason a wall of these
 * beats a wall of names.
 *
 * The grid it draws is the *session's*, never the tile's. A screen dump paints
 * by absolute position and does not reflow, so a 200-column session shown in a
 * 40-column tile would be a screen taken apart rather than a screen made
 * smaller. So the cell is worked out from the shape that came back and the
 * whole grid is scaled into the room there is.
 */
class SnapshotView(context: Context) : View(context) {

    /** The screen to draw, or nothing yet. */
    var preview: Preview? = null
        set(value) {
            field = value
            rows = value?.lines?.associate { it.at.toInt() to it.runs } ?: emptyMap()
            invalidate()
        }

    /** The preview's rows by index, which is what the painter walks. */
    private var rows: Map<Int, List<Run>> = emptyMap()

    private val painter = Painter()

    /**
     * The smallest a cell may be drawn at, in device pixels.
     *
     * A grid scaled to fit whatever shape it has is the honest picture, and on
     * a wide session in a small square it is a smear that says nothing. Below
     * this the scaling stops and the bottom left of the screen is drawn
     * instead, which is where the last thing it printed and the prompt under
     * it are: the top left of a long-running session is the banner it started
     * with.
     */
    private val floor = 2.5f * resources.displayMetrics.density

    override fun onDraw(canvas: Canvas) {
        canvas.drawColor(Palette.GROUND)
        val screen = preview ?: return
        val cols = screen.cols.toInt()
        val lines = screen.rows.toInt()
        if (cols <= 0 || lines <= 0) return

        // The whole grid into the room there is, whichever way round it runs
        // out first. Worked out from one number rather than two, or the cell
        // and the line disagree and the text stops sitting in its cells.
        val cell = maxOf(
            minOf(width.toFloat() / cols, height.toFloat() / lines * ASPECT),
            floor,
        )
        val line = cell / ASPECT
        painter.fitGrid(cell, line)
        // What did not fit is taken off the top rather than the bottom.
        val lift = maxOf(lines * line - height, 0f)
        painter.paint(canvas, rows, width, height, lift, blanks = false)
    }

    private companion object {
        /** How much taller than wide a cell is, matching the terminal's own. */
        const val ASPECT = 0.5f
    }
}
