package dev.manymux.phone

import uniffi.manymux_android.Colour

/**
 * The 256 colours a terminal names by number.
 *
 * A session says `Indexed(1)` and means "red", and which red is the terminal's
 * to decide. The first sixteen are the ones that vary between terminals; the
 * rest are the cube and the greys, which are defined by arithmetic and are the
 * same everywhere.
 */
object Palette {
    /** What text and background are when the session asked for neither. */
    const val TEXT = 0xFFD8D8D8.toInt()
    const val GROUND = 0xFF16181A.toInt()

    private val named = intArrayOf(
        0xFF000000.toInt(), 0xFFCC5555.toInt(), 0xFF55AA55.toInt(), 0xFFCCAA55.toInt(),
        0xFF5577CC.toInt(), 0xFFAA66CC.toInt(), 0xFF55AAAA.toInt(), 0xFFD8D8D8.toInt(),
        0xFF666666.toInt(), 0xFFFF7777.toInt(), 0xFF77DD77.toInt(), 0xFFFFDD77.toInt(),
        0xFF77AAFF.toInt(), 0xFFCC99FF.toInt(), 0xFF77DDDD.toInt(), 0xFFFFFFFF.toInt(),
    )

    fun of(colour: Colour, ifDefault: Int): Int = when (colour) {
        is Colour.Default -> ifDefault
        is Colour.Rgb -> rgb(
            colour.red.toInt(),
            colour.green.toInt(),
            colour.blue.toInt(),
        )
        is Colour.Indexed -> indexed(colour.index.toInt())
    }

    private fun indexed(index: Int): Int = when {
        index < 16 -> named[index]
        index < 232 -> {
            // The 6x6x6 cube, whose levels are not evenly spaced: the step from
            // nothing to the first level is bigger than the ones after it.
            val n = index - 16
            rgb(level(n / 36), level((n / 6) % 6), level(n % 6))
        }
        else -> {
            val grey = 8 + (index - 232) * 10
            rgb(grey, grey, grey)
        }
    }

    private fun level(step: Int): Int = if (step == 0) 0 else 55 + step * 40

    private fun rgb(red: Int, green: Int, blue: Int): Int =
        (0xFF shl 24) or (red shl 16) or (green shl 8) or blue
}
