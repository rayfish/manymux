package dev.manymux.phone

import android.graphics.Canvas
import android.graphics.Paint
import android.graphics.Typeface
import uniffi.manymux_android.Run

/**
 * Draws rows of runs onto a grid, for the session and for its thumbnail.
 *
 * One of these rather than two copies, so a tile and a session cannot disagree
 * about what a run looks like. A run is advanced by its own cell count and
 * never by the font's idea of how wide the text is: a wide character is two
 * cells and one glyph, and measuring the glyph puts the rest of the row out of
 * step with the grid.
 */
class Painter {
    private val ink = Paint(Paint.ANTI_ALIAS_FLAG).apply {
        typeface = Typeface.MONOSPACE
    }
    private val block = Paint()

    /** How wide one cell is, in device pixels. */
    var cell = 0f
        private set

    /** How tall one line is, in device pixels. */
    var line = 0f
        private set

    /** How far down the line the text's baseline sits. */
    private var baseline = 0f

    /** Make the grid the font's own, at [textSize] device pixels. */
    fun fitFont(textSize: Float) {
        ink.textSize = textSize
        cell = ink.measureText("M")
        val metrics = ink.fontMetrics
        line = metrics.descent - metrics.ascent
        baseline = -metrics.ascent
    }

    /**
     * Fit the text to a grid it was handed, as a thumbnail is.
     *
     * The text is set from the cell rather than measured into it, one
     * `measureText` per tile per frame being work for an answer that does not
     * change.
     */
    fun fitGrid(cell: Float, line: Float) {
        ink.textSize = cell / GLYPH_WIDTH
        this.cell = cell
        this.line = line
        baseline = line * BASELINE
    }

    /**
     * Paint each row at its own index, [lift] pixels higher than that, onto
     * a view [width] by [height] pixels.
     *
     * Blank runs are background and nothing else, so their glyphs are skipped
     * when [blanks] is false: a tile drawing every space is a tile paying for
     * a screenful of nothing.
     */
    fun paint(
        canvas: Canvas,
        rows: Map<Int, List<Run>>,
        width: Int,
        height: Int,
        lift: Float = 0f,
        blanks: Boolean = true,
    ) {
        for ((at, runs) in rows) {
            val top = at * line - lift
            if (top > height || top + line < 0f) continue
            var x = 0f
            for (run in runs) {
                if (x > width) break
                val across = run.cells.toInt() * cell
                val background = Palette.of(run.look.background, Palette.GROUND)
                val foreground = Palette.of(run.look.foreground, Palette.TEXT)
                val paper = if (run.look.inverse) foreground else background
                val text = if (run.look.inverse) background else foreground
                if (paper != Palette.GROUND) {
                    block.color = paper
                    canvas.drawRect(x, top, x + across, top + line, block)
                }
                if (blanks || run.text.isNotBlank()) {
                    ink.color = text
                    ink.isFakeBoldText = run.look.bold
                    ink.isUnderlineText = run.look.underline
                    ink.isStrikeThruText = run.look.strikethrough
                    ink.textSkewX = if (run.look.italic) -0.25f else 0f
                    ink.alpha = if (run.look.faint) 0x99 else 0xFF
                    canvas.drawText(run.text, x, top + baseline, ink)
                }
                x += across
            }
        }
    }

    private companion object {
        /** How much wider a monospace glyph's advance is than the point size. */
        const val GLYPH_WIDTH = 0.6f

        /** Where the baseline sits in a handed grid's line, near enough. */
        const val BASELINE = 0.8f
    }
}
