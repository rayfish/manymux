package dev.manymux.phone

import android.content.Context
import android.content.res.Configuration
import android.graphics.Canvas
import android.graphics.Paint
import android.graphics.Typeface
import android.view.Choreographer
import android.view.KeyEvent
import android.view.View
import android.view.inputmethod.BaseInputConnection
import android.view.inputmethod.EditorInfo
import android.view.inputmethod.InputConnection
import uniffi.manymux_android.Attach
import uniffi.manymux_android.Grid
import uniffi.manymux_android.Row
import uniffi.manymux_android.Run

/**
 * How tall one cell's text is, in density-independent pixels.
 *
 * Chosen to land where the pixel size it replaces landed on an ordinary phone,
 * which is about fifty columns: enough for a build log and a prompt, and the
 * width the session reflows to. It is the one number in the app somebody might
 * reasonably want to change, which is what makes it a setting one day rather
 * than a constant.
 */
private const val CELL_DP = 12.5f

/**
 * The session's screen.
 *
 * A plain [View] with an [onDraw], not a composable: the grid has no structure
 * to recompose, it is one surface changing as fast as the session prints, and
 * `Canvas.drawText` on a hardware-accelerated view goes through the platform's
 * own glyph cache. It also lets a frame be skipped outright when nothing
 * changed, which is most frames.
 *
 * Rows are pulled once a frame rather than pushed per byte. Output arriving
 * faster than the screen is drawn coalesces in the emulator on the other side
 * of the boundary, so a burst costs one frame rather than a queue.
 */
class TerminalView(context: Context) : View(context), Choreographer.FrameCallback {

    /** What the session is attached through, once there is one. */
    var attach: Attach? = null
        set(value) {
            field = value
            rows.clear()
            // A fresh attach has been told nothing, whatever the last one knew.
            told = null
            tellGrid()
            invalidate()
        }

    /** Told the shape of the grid whenever it changes, so a caller can say so. */
    var onGrid: ((Grid) -> Unit)? = null

    private val ink = Paint(Paint.ANTI_ALIAS_FLAG).apply {
        typeface = Typeface.MONOSPACE
    }
    private val block = Paint()

    /** Every row the screen has, by index. Replaced as frames arrive. */
    private val rows = HashMap<Int, List<Run>>()
    private var cursorCol = 0
    private var cursorRow = 0
    private var cursorOn = false

    private var cellWidth = 0f
    private var lineHeight = 0f
    private var baseline = 0f

    /** The last grid the far end was told about, so it is told only on a change. */
    private var told: Grid? = null

    init {
        isFocusable = true
        isFocusableInTouchMode = true
        measureCell()
    }

    /**
     * Work out what one cell is, in this screen's pixels.
     *
     * [CELL_DP] and not a pixel count: `Paint.setTextSize` takes device pixels,
     * so a size written as a number is a different physical size on every
     * screen. It showed up on the screens that come in pairs. A foldable's two
     * panels do not have to report the same density, so a size in pixels is
     * text that changes size when the phone is opened, on the one gesture whose
     * whole point is that you are looking at the same thing.
     *
     * Density and not the font scale: the accessibility setting belongs to text
     * that reflows, and a terminal answers a larger one by having fewer columns.
     * Making that somebody's choice is a setting, not a multiplier applied
     * behind their back.
     */
    private fun measureCell() {
        ink.textSize = CELL_DP * resources.displayMetrics.density
        cellWidth = ink.measureText("M")
        val metrics = ink.fontMetrics
        lineHeight = metrics.descent - metrics.ascent
        baseline = -metrics.ascent
    }

    /**
     * A screen that changed shape, unfolded, or moved to a display of another
     * density.
     *
     * Reached at all because the activity declares these rather than being
     * recreated for them: the attach outlives the fold, which is the whole
     * point of it.
     */
    override fun onConfigurationChanged(config: Configuration?) {
        super.onConfigurationChanged(config)
        measureCell()
        rows.clear()
        tellGrid()
    }

    /** Tell the far end the shape, if it is not the shape it was already told. */
    private fun tellGrid() {
        val grid = grid()
        android.util.Log.i("manymux", "grid: ${grid.cols}x${grid.rows} view=${width}x$height")
        if (grid == told) return
        told = grid
        attach?.resize(grid)
        onGrid?.invoke(grid)
    }

    /** How many cells fit, which is what the far end is told. */
    private fun grid(): Grid {
        val across = if (cellWidth > 0) (width / cellWidth).toInt() else 0
        val down = if (lineHeight > 0) (height / lineHeight).toInt() else 0
        return Grid(
            across.coerceAtLeast(1).toUShort(),
            down.coerceAtLeast(1).toUShort(),
        )
    }

    override fun onSizeChanged(w: Int, h: Int, oldw: Int, oldh: Int) {
        tellGrid()
    }

    override fun onAttachedToWindow() {
        super.onAttachedToWindow()
        Choreographer.getInstance().postFrameCallback(this)
    }

    override fun onDetachedFromWindow() {
        super.onDetachedFromWindow()
        Choreographer.getInstance().removeFrameCallback(this)
    }

    /** Once a frame: take whatever changed and draw only if something did. */
    override fun doFrame(nanos: Long) {
        val session = attach
        if (session != null) {
            val frame = session.takeFrame()
            val moved = frame.cursor.col.toInt() != cursorCol ||
                frame.cursor.row.toInt() != cursorRow ||
                frame.cursor.visible != cursorOn
            if (frame.changed.isNotEmpty() || moved) {
                for (row: Row in frame.changed) {
                    rows[row.at.toInt()] = row.runs
                }
                cursorCol = frame.cursor.col.toInt()
                cursorRow = frame.cursor.row.toInt()
                cursorOn = frame.cursor.visible
                invalidate()
            }
        }
        Choreographer.getInstance().postFrameCallback(this)
    }

    override fun onDraw(canvas: Canvas) {
        canvas.drawColor(Palette.GROUND)
        for ((at, runs) in rows) {
            var x = 0f
            val top = at * lineHeight
            for (run in runs) {
                // The run's own cell count, never the font's idea of how wide
                // the text is: a wide character is two cells and one glyph, and
                // measuring the glyph puts the rest of the row out of step.
                val across = run.cells.toInt() * cellWidth
                val background = Palette.of(run.look.background, Palette.GROUND)
                val foreground = Palette.of(run.look.foreground, Palette.TEXT)
                val paper = if (run.look.inverse) foreground else background
                val text = if (run.look.inverse) background else foreground
                if (paper != Palette.GROUND) {
                    block.color = paper
                    canvas.drawRect(x, top, x + across, top + lineHeight, block)
                }
                ink.color = text
                ink.isFakeBoldText = run.look.bold
                ink.isUnderlineText = run.look.underline
                ink.isStrikeThruText = run.look.strikethrough
                ink.textSkewX = if (run.look.italic) -0.25f else 0f
                ink.alpha = if (run.look.faint) 0x99 else 0xFF
                canvas.drawText(run.text, x, top + baseline, ink)
                x += across
            }
        }
        if (cursorOn) {
            block.color = Palette.TEXT
            block.alpha = 0x88
            canvas.drawRect(
                cursorCol * cellWidth,
                cursorRow * lineHeight,
                (cursorCol + 1) * cellWidth,
                (cursorRow + 1) * lineHeight,
                block,
            )
            block.alpha = 0xFF
        }
    }

    // ---- the keyboard -------------------------------------------------

    /** Whether the next key is a control chord, set by the extra-keys row. */
    var control = false

    override fun onCheckIsTextEditor(): Boolean = true

    override fun onCreateInputConnection(out: EditorInfo): InputConnection {
        // `TYPE_NULL` is what asks the IME to send key events rather than
        // composing text at us, which is the difference between a terminal and
        // a text field. Not every keyboard obeys it, so `commitText` below is
        // the other half.
        out.inputType = EditorInfo.TYPE_NULL
        out.imeOptions = EditorInfo.IME_FLAG_NO_EXTRACT_UI or
            EditorInfo.IME_FLAG_NO_FULLSCREEN or
            EditorInfo.IME_ACTION_NONE
        return object : BaseInputConnection(this, true) {
            override fun commitText(text: CharSequence?, position: Int): Boolean {
                text?.toString()?.let { type(it) }
                return true
            }

            override fun setComposingText(text: CharSequence?, position: Int): Boolean = true

            override fun deleteSurroundingText(before: Int, after: Int): Boolean {
                repeat(before) { send(byteArrayOf(0x7f)) }
                return true
            }

            override fun sendKeyEvent(event: KeyEvent?): Boolean {
                if (event != null && event.action == KeyEvent.ACTION_DOWN) {
                    return onKeyDown(event.keyCode, event)
                }
                return true
            }
        }
    }

    override fun onKeyDown(code: Int, event: KeyEvent): Boolean {
        val bytes = encode(code, event) ?: return super.onKeyDown(code, event)
        send(bytes)
        return true
    }

    /** What a key press is, on the wire. */
    private fun encode(code: Int, event: KeyEvent): ByteArray? = when (code) {
        KeyEvent.KEYCODE_ENTER -> byteArrayOf(0x0d)
        KeyEvent.KEYCODE_DEL -> byteArrayOf(0x7f)
        KeyEvent.KEYCODE_TAB -> byteArrayOf(0x09)
        KeyEvent.KEYCODE_ESCAPE -> byteArrayOf(0x1b)
        // The plain spellings. A session in application cursor mode wants the
        // SS3 ones instead, which this build does not know it is in: the
        // emulator answers for that and the answer does not cross the boundary
        // yet.
        KeyEvent.KEYCODE_DPAD_UP -> "\u001b[A".toByteArray()
        KeyEvent.KEYCODE_DPAD_DOWN -> "\u001b[B".toByteArray()
        KeyEvent.KEYCODE_DPAD_RIGHT -> "\u001b[C".toByteArray()
        KeyEvent.KEYCODE_DPAD_LEFT -> "\u001b[D".toByteArray()
        // Volume down is a second ctrl, the way Termux does it: a phone
        // keyboard has none, and a good half of what anybody types at a shell
        // is one.
        KeyEvent.KEYCODE_VOLUME_DOWN -> {
            control = true
            ByteArray(0)
        }
        else -> {
            val typed = event.unicodeChar
            if (typed == 0) null else charOf(typed.toChar())
        }
    }

    /** Text from a keyboard that composes rather than sending key events. */
    fun type(text: String) {
        if (control && text.isNotEmpty()) {
            send(charOf(text[0]))
            if (text.length > 1) send(text.substring(1).toByteArray())
        } else {
            send(text.toByteArray())
        }
    }

    /** One character, under whatever the extra-keys row is holding. */
    private fun charOf(character: Char): ByteArray {
        if (control) {
            control = false
            // Ctrl-A is 1 and Ctrl-] is 0x1d: the low five bits of the letter,
            // which is what a terminal has always sent.
            val chord = character.uppercaseChar().code and 0x1f
            return byteArrayOf(chord.toByte())
        }
        return character.toString().toByteArray()
    }

    fun send(bytes: ByteArray) {
        if (bytes.isEmpty()) return
        attach?.send(bytes)
    }
}
