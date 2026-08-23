package dev.manymux.phone

import android.content.Context
import android.content.res.Configuration
import android.graphics.Canvas
import android.graphics.Paint
import android.graphics.Typeface
import android.view.Choreographer
import android.view.GestureDetector
import android.view.KeyEvent
import android.view.MotionEvent
import android.view.View
import android.view.inputmethod.BaseInputConnection
import android.view.inputmethod.EditorInfo
import android.view.inputmethod.InputConnection
import android.view.inputmethod.InputMethodManager
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
 * What is left of a fling's speed after a sixtieth of a second.
 *
 * Spent against the frame's own length rather than per frame, so the same
 * flick covers the same ground on a 120Hz screen as on a 60Hz one.
 */
private const val DECAY = 0.92f

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
            history.clear()
            viewing = false
            viewOpen = false
            looking = false
            speed = 0f
            pending = 0f
            said = false
            // A fresh attach has been told nothing, whatever the last one knew.
            told = null
            tellGrid()
            invalidate()
        }

    /** Told the shape of the grid whenever it changes, so a caller can say so. */
    var onGrid: ((Grid) -> Unit)? = null

    /**
     * Told when somebody scrolls on a host that cannot answer for it.
     *
     * A key that quietly does nothing is the thing `Response::Attached`'s
     * capability flags exist to stop, and a gesture is worse than a key: there
     * is nothing on the screen saying it was ever a gesture. So it is said
     * where it was made, once, rather than on the bar of every attach to such
     * a host.
     */
    var onCannotScroll: (() -> Unit)? = null

    private val ink = Paint(Paint.ANTI_ALIAS_FLAG).apply {
        typeface = Typeface.MONOSPACE
    }
    private val block = Paint()

    /** Every row the screen has, by index. Replaced as frames arrive. */
    private val rows = HashMap<Int, List<Run>>()
    private var cursorCol = 0
    private var cursorRow = 0
    private var cursorOn = false

    /**
     * Every row of the view over the host's history, when one is up.
     *
     * Kept beside the screen's rows rather than instead of them: the session
     * goes on running and its rows go on arriving while somebody reads what it
     * printed a minute ago, so coming back is a redraw and not a round trip.
     */
    private val history = HashMap<Int, List<Run>>()

    /** Whether the view is what is drawn. */
    private var viewing = false

    /** Whether the view is up at all, which it is before it has anything. */
    private var viewOpen = false
    private var viewFrom = 0L
    private var viewTotal = 0L

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
        android.util.Log.i(
            "manymux",
            "grid: ${grid.cols}x${grid.rows} view=${width}x$height" +
                " told=${told?.cols}x${told?.rows} attached=${attach != null}",
        )
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
        // A keyboard asked for before there was a view to type into, now that
        // there is one. Posted rather than asked for here, because this is the
        // middle of a layout and taking focus starts another.
        if (wanted) {
            wanted = false
            post { openKeyboard() }
        }
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
            coast(nanos)
            var redraw = false
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
                // The session is still painting behind an open view, and its
                // rows are kept for the moment somebody comes back to it. What
                // it must not do is paint over what they are reading.
                redraw = redraw || !viewing
            }
            // Asked for only once somebody has reached for the history, and
            // until the answer says the view is closed again. A session
            // nobody scrolls is the ordinary case, and it should not pay a
            // call across the boundary sixty times a second to be told so.
            if (looking || viewOpen) {
                val window = session.takeWindow()
                // A view opened again is a view opened at the bottom, so the
                // rows the last one left behind say nothing about this one.
                if (window.open && !viewOpen) history.clear()
                viewOpen = window.open
                if (!window.open) looking = false
                viewFrom = window.from.toLong()
                viewTotal = window.total.toLong()
                for (row: Row in window.changed) {
                    history[row.at.toInt()] = row.runs
                }
                val showing = window.open && window.showing
                redraw = redraw || window.changed.isNotEmpty() || showing != viewing
                viewing = showing
            }
            if (redraw) invalidate()
        }
        lastFrame = nanos
        Choreographer.getInstance().postFrameCallback(this)
    }

    override fun onDraw(canvas: Canvas) {
        canvas.drawColor(Palette.GROUND)
        for ((at, runs) in if (viewing) history else rows) {
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
        if (viewing) {
            drawMark(canvas)
            return
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

    /**
     * Where in the history the view is, down the right-hand edge.
     *
     * A phone has no mark row of its own to say it, and a screen of old output
     * looks exactly like a screen of new output: without this the only sign
     * that a session had stopped moving was that it had stopped moving.
     */
    private fun drawMark(canvas: Canvas) {
        if (viewTotal <= 0L) return
        val rows = (height / lineHeight).toInt().coerceAtLeast(1)
        val reach = (viewTotal - rows).coerceAtLeast(1L)
        val down = 1f - (viewFrom.coerceIn(0L, reach).toFloat() / reach)
        val bar = dp(3f)
        val tall = (height * (rows.toFloat() / viewTotal)).coerceAtLeast(dp(24f))
        val top = (height - tall) * down
        block.color = Palette.TEXT
        block.alpha = 0x66
        canvas.drawRect(width - bar * 2f, top, width - bar, top + tall, block)
        block.alpha = 0xFF
    }

    private fun dp(value: Float) = value * resources.displayMetrics.density

    // ---- the keyboard -------------------------------------------------

    /** Whether the next key is a control chord, set by the extra-keys row. */
    var control = false

    /** A keyboard asked for before this view had a size to be focused at. */
    private var wanted = false

    /**
     * Ask for the keyboard.
     *
     * There has to be a way back to it, and on a phone the keyboard's own key
     * for going away is always there: pressed once, a session became something
     * you could read and not type in, for the rest of the attach. A terminal is
     * not a form and has no field to tap on, so the surface itself is what is
     * tapped, which is what every terminal on this platform does, and the row
     * of keys under it says so as well for a hand that has just used that row.
     *
     * A request made before the first layout waits for it. `View.canTakeFocus`
     * refuses a view of no size, and this is asked for in the same breath as
     * the screen going up, which is before any of it has been measured: the
     * focus went nowhere, and the keyboard that came up anyway came up over a
     * window that had not been told to make room for it, since the request
     * that lifts a screen is the one made by the view being typed into.
     */
    fun openKeyboard() {
        if (width == 0 || height == 0) {
            wanted = true
            return
        }
        requestFocus()
        val ime = context.getSystemService(Context.INPUT_METHOD_SERVICE) as InputMethodManager
        ime.showSoftInput(this, InputMethodManager.SHOW_IMPLICIT)
    }

    /** Put it away, for a surface of the app's own that wants the room. */
    fun closeKeyboard() {
        wanted = false
        val ime = context.getSystemService(Context.INPUT_METHOD_SERVICE) as InputMethodManager
        ime.hideSoftInputFromWindow(windowToken, 0)
    }

    override fun onTouchEvent(event: MotionEvent): Boolean {
        gestures.onTouchEvent(event)
        if (event.actionMasked == MotionEvent.ACTION_UP ||
            event.actionMasked == MotionEvent.ACTION_CANCEL
        ) {
            // A gesture that ended at the live screen is somebody done with
            // the history, and leaving it is what puts the keyboard back over
            // a session rather than over a page of old output.
            if (speed == 0f) settle()
        }
        // Taken whatever it is, or the up that ends the tap goes to whoever
        // took the down.
        return true
    }

    override fun performClick(): Boolean {
        super.performClick()
        // A tap while the history is up is the way back to the session, and
        // the obvious one to reach for: the keyboard is already there, and
        // what is wanted is the screen under it.
        if (viewOpen) {
            attach?.closeView()
            speed = 0f
            pending = 0f
        } else {
            openKeyboard()
        }
        return true
    }

    // ---- the history ----------------------------------------------------

    /** Pixels dragged that have not yet added up to a whole line. */
    private var pending = 0f

    /** A fling's speed, in pixels a second, or zero while nothing is coasting. */
    private var speed = 0f
    private var lastFrame = 0L

    private val gestures = GestureDetector(
        context,
        object : GestureDetector.SimpleOnGestureListener() {
            override fun onDown(event: MotionEvent): Boolean {
                // A hand on the screen stops a fling, which is what every
                // list on this platform does and what somebody reaching to
                // stop one expects.
                speed = 0f
                pending = 0f
                return true
            }

            override fun onSingleTapUp(event: MotionEvent): Boolean {
                performClick()
                return true
            }

            override fun onScroll(
                from: MotionEvent?,
                to: MotionEvent,
                acrossBy: Float,
                downBy: Float,
            ): Boolean {
                // `downBy` counts the way the content moves, so pulling the
                // screen down is negative and is what reaches back into the
                // history.
                drag(-downBy)
                return true
            }

            override fun onFling(
                from: MotionEvent?,
                to: MotionEvent,
                across: Float,
                down: Float,
            ): Boolean {
                speed = down
                return true
            }
        },
    )

    /**
     * Move the view by however many whole lines this much dragging comes to.
     *
     * The remainder is kept rather than dropped: a slow drag is a run of
     * reports each worth a fraction of a line, and rounding every one of them
     * to nothing is a screen that does not move until the hand moves fast.
     */
    private fun drag(pixels: Float) {
        val session = attach ?: return
        if (lineHeight <= 0f) return
        if (!session.scrolls()) {
            if (!said) {
                said = true
                onCannotScroll?.invoke()
            }
            return
        }
        pending += pixels
        val lines = (pending / lineHeight).toInt()
        if (lines == 0) return
        pending -= lines * lineHeight
        if (lines > 0) {
            looking = true
            session.scrollUp(lines.toULong())
        } else {
            session.scrollDown((-lines).toULong())
        }
    }

    /** Whether the view is worth asking about, which reaching back makes it. */
    private var looking = false

    /** Whether this attach has already been told the host cannot scroll. */
    private var said = false

    /**
     * Carry a fling on, and stop it where there is nothing left to move to.
     *
     * The decay is per second rather than per frame, or the same flick would
     * travel twice as far on a 120Hz screen as on a 60Hz one.
     */
    private fun coast(nanos: Long) {
        if (speed == 0f) return
        val seconds = if (lastFrame == 0L) 0f else (nanos - lastFrame) / 1e9f
        if (seconds <= 0f || seconds > 0.25f) return
        drag(speed * seconds)
        speed *= Math.pow(DECAY.toDouble(), seconds.toDouble() * 60).toFloat()
        val ends = viewFrom == 0L && speed < 0f ||
            viewTotal > 0L && viewFrom >= viewTotal - (height / lineHeight).toInt() && speed > 0f
        if (Math.abs(speed) < Math.max(lineHeight * 2f, 1f) || ends) {
            speed = 0f
            pending = 0f
            settle()
        }
    }

    /** A gesture that ended at the live screen leaves the view behind. */
    private fun settle() {
        if (viewOpen && viewFrom == 0L) attach?.closeView()
    }

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
        // Typing is asking for the session and not for the history: what is
        // typed lands in a shell that has moved on since the lines being read
        // were printed, and a screen that stayed on them would be one where
        // nothing anybody typed appeared to do anything.
        if (viewOpen) {
            attach?.closeView()
            speed = 0f
            pending = 0f
        }
        attach?.send(bytes)
    }
}
