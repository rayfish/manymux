package dev.manymux.phone

import android.content.Context
import android.graphics.Canvas
import android.graphics.Paint
import android.graphics.Typeface
import android.view.View
import uniffi.manymux_android.Preview
import uniffi.manymux_android.Row

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
            invalidate()
        }

    private val ink = Paint(Paint.ANTI_ALIAS_FLAG).apply {
        typeface = Typeface.MONOSPACE
    }
    private val block = Paint()

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
        val rows = screen.rows.toInt()
        if (cols <= 0 || rows <= 0) return

        // The whole grid into the room there is, whichever way round it runs
        // out first. Worked out from one number rather than two, or the cell
        // and the line disagree and the text stops sitting in its cells.
        val cell = maxOf(
            minOf(width.toFloat() / cols, height.toFloat() / rows * ASPECT),
            floor,
        )
        val line = cell / ASPECT
        ink.textSize = cell / WIDTH
        // What did not fit is taken off the top rather than the bottom.
        val lift = maxOf(rows * line - height, 0f)

        for (row: Row in screen.lines) {
            val top = row.at.toInt() * line - lift
            if (top > height) break
            if (top + line < 0f) continue
            var x = 0f
            for (run in row.runs) {
                val across = run.cells.toInt() * cell
                if (x > width) break
                val background = Palette.of(run.look.background, Palette.GROUND)
                val foreground = Palette.of(run.look.foreground, Palette.TEXT)
                val paper = if (run.look.inverse) foreground else background
                val text = if (run.look.inverse) background else foreground
                if (paper != Palette.GROUND) {
                    block.color = paper
                    canvas.drawRect(x, top, x + across, top + line, block)
                }
                // Blank runs are the greater part of most screens and are the
                // background and nothing else, so the glyphs are skipped: a
                // tile drawing every space is a tile paying for a screenful of
                // nothing.
                if (run.text.isNotBlank()) {
                    ink.color = text
                    ink.isFakeBoldText = run.look.bold
                    ink.alpha = if (run.look.faint) 0x99 else 0xFF
                    canvas.drawText(run.text, x, top + line * BASELINE, ink)
                }
                x += across
            }
        }
    }

    private companion object {
        /** How much taller than wide a cell is, matching the terminal's own. */
        const val ASPECT = 0.5f

        /**
         * How much wider a monospace glyph's advance is than the point size.
         * The text is set from the cell rather than measured into it, one
         * `measureText` per tile per frame being work for an answer that does
         * not change.
         */
        const val WIDTH = 0.6f

        /** Where the baseline sits in the line, near enough for this size. */
        const val BASELINE = 0.8f
    }
}
