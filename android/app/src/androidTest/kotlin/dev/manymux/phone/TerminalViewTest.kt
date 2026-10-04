package dev.manymux.phone

import android.content.ClipboardManager
import android.content.Context
import android.os.SystemClock
import android.graphics.Rect
import android.view.ViewGroup
import android.widget.TextView
import androidx.test.core.app.ActivityScenario
import androidx.test.platform.app.InstrumentationRegistry
import androidx.test.ext.junit.runners.AndroidJUnit4
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import android.view.MotionEvent
import android.view.View
import android.view.ActionMode
import uniffi.manymux_android.Colour
import uniffi.manymux_android.Look
import uniffi.manymux_android.Machine
import uniffi.manymux_android.Running
import uniffi.manymux_android.Wall
import uniffi.manymux_android.Run

@RunWith(AndroidJUnit4::class)
class TerminalViewTest {
    private val instrumentation = InstrumentationRegistry.getInstrumentation()

    @Test
    fun longPressAndDragCopiesWideText() {
        val scenario = ActivityScenario.launch(MainActivity::class.java)
        try {
            lateinit var activity: MainActivity
            scenario.onActivity { activity = it }
            lateinit var terminal: TerminalView
            instrumentation.runOnMainSync {
                terminal = TerminalView(activity)
                val show = MainActivity::class.java.getDeclaredMethod("show", View::class.java)
                show.isAccessible = true
                show.invoke(activity, terminal)
                val look = Look(
                    foreground = Colour.Default,
                    background = Colour.Default,
                    bold = false, faint = false, italic = false, underline = false,
                    strikethrough = false, blink = false, inverse = false,
                )
                @Suppress("UNCHECKED_CAST")
                val rows = field(terminal, "rows") as MutableMap<Int, List<Run>>
                rows[0] = listOf(Run("ab界cd", 6u, look))
                terminal.invalidate()
            }
            instrumentation.waitForIdleSync()
            val cell = field(terminal, "cellWidth") as Float
            val line = field(terminal, "lineHeight") as Float
            val down = SystemClock.uptimeMillis()
            touch(terminal, down, MotionEvent.ACTION_DOWN, cell * 2.5f, line / 2)
            SystemClock.sleep(800)
            touch(terminal, down, MotionEvent.ACTION_MOVE, cell * 5.5f, line / 2)
            touch(terminal, down, MotionEvent.ACTION_UP, cell * 5.5f, line / 2)
            instrumentation.waitForIdleSync()
            assertNotNull("Long press should open selection", field(terminal, "selectionRows"))
            instrumentation.runOnMainSync {
                val mode = field(terminal, "selectionMode") as ActionMode
                assertNotNull("Selection should offer Copy", mode.menu.findItem(1))
                assertTrue(mode.menu.performIdentifierAction(1, 0))
            }
            instrumentation.waitForIdleSync()
            instrumentation.runOnMainSync {
                val clipboard = activity.getSystemService(Context.CLIPBOARD_SERVICE) as ClipboardManager
                assertEquals("界cd", clipboard.primaryClip?.getItemAt(0)?.text?.toString())
                assertNull("Copy should return to the live screen", field(terminal, "selectionRows"))
            }
        } finally {
            scenario.close()
        }
    }

    @Test
    fun threeSessionsHaveVisibleCaptionsAndFixedThumbnailHeights() {
        val scenario = ActivityScenario.launch(MainActivity::class.java)
        try {
            lateinit var root: View
            scenario.onActivity { activity ->
                val tiles = MainActivity::class.java.getDeclaredField("tiles")
                tiles.isAccessible = true
                tiles.setBoolean(activity, true)
                val machine = Machine("host", 22u, "user")
                val wall = Wall(
                    listOf("one", "two", "three").map { Running(it, "", "shell", 0u, 0u) },
                    emptyList(), true,
                )
                val sessions = MainActivity::class.java.getDeclaredMethod("sessions", Machine::class.java, Wall::class.java)
                sessions.isAccessible = true
                val body = sessions.invoke(activity, machine, wall) as View
                val overview = MainActivity::class.java.getDeclaredMethod("overview", Machine::class.java, View::class.java)
                overview.isAccessible = true
                root = overview.invoke(activity, machine, body) as View
                val show = MainActivity::class.java.getDeclaredMethod("show", View::class.java)
                show.isAccessible = true
                show.invoke(activity, root)
            }
            instrumentation.waitForIdleSync()
            scenario.onActivity { activity ->
                val children = descendants(root)
                val snapshots = children.filterIsInstance<SnapshotView>()
                assertEquals(3, snapshots.size)
                val expected = (110 * activity.resources.displayMetrics.density).toInt()
                for (snapshot in snapshots) assertEquals("Thumbnail must keep its fixed height", expected, snapshot.height)
                for (name in listOf("one", "two", "three")) {
                    val caption = children.filterIsInstance<TextView>().single { it.text.toString() == name }
                    val visible = Rect()
                    assertTrue("Session caption must be visible: $name", caption.getGlobalVisibleRect(visible))
                    assertEquals("Caption must not be clipped", caption.height, visible.height())
                }
            }
        } finally {
            scenario.close()
        }
    }

    @Test
    fun extraKeysHaveTwoRowsAndDesktopArrowPositions() {
        val scenario = ActivityScenario.launch(MainActivity::class.java)
        try {
            lateinit var keys: ViewGroup
            scenario.onActivity { activity ->
                val method = MainActivity::class.java.getDeclaredMethod("extraKeys", TerminalView::class.java)
                method.isAccessible = true
                keys = method.invoke(activity, TerminalView(activity)) as ViewGroup
                val show = MainActivity::class.java.getDeclaredMethod("show", View::class.java)
                show.isAccessible = true
                show.invoke(activity, keys)
            }
            instrumentation.waitForIdleSync()
            scenario.onActivity { activity ->
                assertEquals(2, keys.childCount)
                val buttons = descendants(keys).filterIsInstance<TextView>()
                assertEquals(9, buttons.size)
                fun position(name: String): Rect {
                    val button = buttons.single { it.contentDescription == name }
                    assertEquals((48 * activity.resources.displayMetrics.density).toInt(), button.height)
                    val rect = Rect()
                    assertTrue(button.getGlobalVisibleRect(rect))
                    return rect
                }
                val up = position("up")
                val down = position("down")
                val left = position("left")
                val right = position("right")
                assertEquals(up.centerX(), down.centerX())
                assertTrue(up.bottom <= down.top)
                assertEquals(left.top, down.top)
                assertEquals(right.top, down.top)
                assertTrue(left.right <= down.left)
                assertTrue(down.right <= right.left)
                for (name in listOf("esc", "tab", "ctrl", "paste", "keyboard")) position(name)
            }
        } finally {
            scenario.close()
        }
    }

    @Test
    fun ctrlHighlightFollowsKeyboardConsumptionAndReset() {
        val scenario = ActivityScenario.launch(MainActivity::class.java)
        try {
            scenario.onActivity { activity ->
                val terminal = TerminalView(activity)
                val method = MainActivity::class.java.getDeclaredMethod("extraKeys", TerminalView::class.java)
                method.isAccessible = true
                val keys = method.invoke(activity, terminal) as View
                val ctrl = descendants(keys).filterIsInstance<TextView>().single { it.text == "ctrl" }
                val normal = ctrl.currentTextColor
                fun arm() {
                    ctrl.performClick()
                    assertTrue(terminal.control)
                    assertTrue(ctrl.isSelected)
                    assertTrue(normal != ctrl.currentTextColor)
                }
                fun released() {
                    assertTrue(!terminal.control)
                    assertTrue(!ctrl.isSelected)
                    assertEquals(normal, ctrl.currentTextColor)
                }
                arm()
                terminal.onCreateInputConnection(android.view.inputmethod.EditorInfo()).commitText("c", 1)
                released()
                arm()
                terminal.onKeyDown(android.view.KeyEvent.KEYCODE_C,
                    android.view.KeyEvent(android.view.KeyEvent.ACTION_DOWN, android.view.KeyEvent.KEYCODE_C))
                released()
                arm()
                ctrl.performClick()
                released()
                arm()
                terminal.attach = null
                released()
            }
        } finally {
            scenario.close()
        }
    }

    private fun descendants(view: View): List<View> = listOf(view) +
        if (view is ViewGroup) (0 until view.childCount).flatMap { descendants(view.getChildAt(it)) }
        else emptyList()

    private fun field(view: TerminalView, name: String): Any? {
        val field = TerminalView::class.java.getDeclaredField(name)
        field.isAccessible = true
        return field.get(view)
    }

    private fun touch(view: TerminalView, down: Long, action: Int, x: Float, y: Float) {
        instrumentation.runOnMainSync {
            val event = MotionEvent.obtain(down, SystemClock.uptimeMillis(), action, x, y, 0)
            view.onTouchEvent(event)
            event.recycle()
        }
    }
}
