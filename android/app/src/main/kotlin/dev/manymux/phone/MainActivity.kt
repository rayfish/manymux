package dev.manymux.phone

import android.app.Activity
import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.content.SharedPreferences
import android.content.res.ColorStateList
import android.graphics.Color
import android.graphics.Rect
import android.graphics.Typeface
import android.graphics.drawable.GradientDrawable
import android.graphics.drawable.RippleDrawable
import android.graphics.drawable.StateListDrawable
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.text.InputType
import android.text.TextUtils
import android.view.Gravity
import android.view.View
import android.view.ViewGroup.LayoutParams.MATCH_PARENT
import android.view.ViewGroup.LayoutParams.WRAP_CONTENT
import android.view.WindowInsets
import android.view.WindowInsetsAnimation
import android.view.inputmethod.InputMethodManager
import android.widget.EditText
import android.widget.LinearLayout
import android.widget.PopupMenu
import android.widget.ScrollView
import android.widget.TextView
import android.widget.Toast
import android.window.OnBackInvokedDispatcher
import java.util.concurrent.Executors
import uniffi.manymux_android.Attach
import uniffi.manymux_android.Direction
import uniffi.manymux_android.Grid
import uniffi.manymux_android.Machine
import uniffi.manymux_android.Phone
import uniffi.manymux_android.Preview
import uniffi.manymux_android.Running
import uniffi.manymux_android.State
import uniffi.manymux_android.Trouble
import uniffi.manymux_android.Wall

/**
 * The whole app: a machine, what is running on it, and one of those on the
 * screen.
 *
 * There is no mode key here, which is the idea the rest of it hangs on. On a
 * desktop `Ctrl-]` exists because a terminal has one keyboard and the session
 * wants every key of it; a phone has chrome the session does not own, so the
 * key goes straight through and the verbs behind it become things you press.
 *
 * The shape that follows is the platform's own: what is running is the main
 * screen, a session is a screen on top of it, and back is how you come out.
 * That is already the gesture people have, since a swipe in from the left edge
 * *is* back, so it needs no drawer and nothing to learn. What made leaving
 * expensive was never the navigation, it was that leaving used to take the ssh
 * connection with it. It no longer does (`machine::Connections`).
 *
 * Every size here is worked out from the screen's density through [dp].
 * Written as plain numbers, as they were, they are device pixels: on an
 * ordinary phone that is a third of the spacing intended, which is most of what
 * made this look like a form somebody threw together. `TextView.textSize` is
 * the exception and needs no conversion, being in scaled points already.
 */
class MainActivity : Activity() {

    private lateinit var phone: Phone

    /** Anything that talks to a machine, off the thread drawing the screen. */
    private val elsewhere = Executors.newSingleThreadExecutor()
    private val here = Handler(Looper.getMainLooper())

    private var attach: Attach? = null
    private var terminal: TerminalView? = null

    /** The machine being looked at, kept so the list can be reopened. */
    private var machine: Machine? = null
    private var homeDetails = false

    /** The bar's own clock, kept so it can be stopped. */
    private var ticking: Runnable? = null

    /**
     * What was running the last time a machine answered.
     *
     * Kept so the switcher over a session opens on something rather than on a
     * round trip: asking takes a connection, a ladder and an answer, which is
     * a second of nothing where a list was expected, and the list somebody
     * just came through is the same list. It is asked for again in the
     * background whenever a session is opened, so the copy behind the button
     * is one listing old at worst rather than as old as the screen.
     */
    private var seen: List<Running> = emptyList()

    /**
     * The whole of the last answer, kept so the view can be redrawn from it.
     *
     * [seen] is the half the switcher wants and this is the half the main
     * screen wants: the screens, and whether the machine would hand them over
     * at all. Held because changing how they are drawn must not be a round
     * trip, a button that takes a second of ssh to redraw the same sessions
     * being a button nobody presses twice.
     */
    private var answered: Wall? = null

    /**
     * Whether the sessions are drawn as screens rather than as a list.
     *
     * Which of the two is more use depends on how many there are: half a dozen
     * tiles say what is happening at a glance, and twenty of them are a wall to
     * scroll past when what you wanted was a name. So it is a choice, and it is
     * remembered, being about how somebody reads rather than about the machine
     * they are reading. A machine too old to be peeked has no choice to make
     * and is drawn as a list whatever this says.
     */
    private var tiles = true

    /** Whether a listing nobody is waiting for is already out. */
    private var asking = false

    /** Whether the keyboard is moving, and the padding is the animation's. */
    private var lifting = false

    /** Whether everything has been let go of, so late work stays away. */
    private var gone = false

    private var surface = 0L
    private var wallRefresh: Runnable? = null

    private lateinit var hosts: HostStore
    private lateinit var remembered: SharedPreferences

    override fun onCreate(saved: Bundle?) {
        super.onCreate(saved)

        // Say outright that laying this window out is the app's job, rather
        // than inheriting an answer from whichever Android is underneath.
        // Android 15 decides it for an app targeting SDK 35 or above and hands
        // the job over; every version before it keeps the job and resizes the
        // window for the keyboard itself, which is what `adjustResize` in the
        // manifest asks for. Left to the platform that is two behaviours for
        // one screen to be right under, and the one this was written against
        // is not the one most phones are running. Said here it is one.
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
            window.setDecorFitsSystemWindows(false)
        }

        // App-private storage: the key this device is known by lives here and
        // nowhere else.
        phone = Phone.keptIn(filesDir.absolutePath)
        remembered = getSharedPreferences("manymux", MODE_PRIVATE)
        hosts = HostStore(remembered)
        tiles = remembered.getBoolean("tiles", true)
        takeBack()

        showMachine()

    }

    override fun onDestroy() {
        super.onDestroy()
        // The session goes on running on the machine. This only stops watching
        // it, which is the whole point of the thing.
        gone = true
        wallRefresh?.let { here.removeCallbacks(it) }
        letGo()
        phone.close()
        elsewhere.shutdownNow()
    }

    override fun onWindowFocusChanged(focused: Boolean) {
        super.onWindowFocusChanged(focused)
        if (focused && attach == null && answered != null && wallRefresh != null) {
            wallRefresh?.let { here.removeCallbacks(it); here.post(it) }
        }
    }

    // ---- back ------------------------------------------------------------

    /**
     * Take the back gesture, in whichever way this Android delivers it.
     *
     * The app used to ask for the old delivery with
     * `android:enableOnBackInvokedCallback="false"` and read `onBackPressed`.
     * That flag stops being honoured for an app targeting SDK 36: predictive
     * back is on regardless and the deprecated callbacks are no longer called,
     * so back closed the app rather than leaving the session, which is the one
     * gesture the whole shape depends on. Registered here, it works on both
     * sides of that line: the dispatcher from API 33, `onBackPressed` before.
     */
    private fun takeBack() {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            onBackInvokedDispatcher.registerOnBackInvokedCallback(
                OnBackInvokedDispatcher.PRIORITY_DEFAULT,
            ) { goBack() }
        }
    }

    /** Leave the session for the list, or the list for whatever came before. */
    private fun goBack() {
        if (attach == null) {
            if (machine != null || homeDetails) showMachine() else finish()
            return
        }
        letGo()
        machine?.let { listSessions(it) } ?: showMachine()
    }

    @Deprecated("Reached only below API 33, where the dispatcher does not exist.")
    @Suppress("MissingSuperCall")
    override fun onBackPressed() {
        goBack()
    }

    // ---- the machine ---------------------------------------------------

    private fun lastMachine(): Machine? = hosts.all().firstOrNull()

    private fun remember(machine: Machine) = hosts.remember(machine)

    /** Saved hosts are the home screen; setup has its own pages. */
    private fun showMachine() {
        hideKeyboard()
        machine = null
        homeDetails = false
        answered = null
        val saved = hosts.all()
        val body = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(dp(20), dp(8), dp(20), dp(24))
            if (saved.isEmpty()) {
                addView(TextView(this@MainActivity).apply {
                    text = "No hosts yet"
                    textSize = 20f
                    setTextColor(colour(R.color.text))
                    setPadding(dp(4), dp(36), dp(4), dp(8))
                })
                addView(note("Save an SSH host to open its running sessions."))
            }
            val columns = if (resources.configuration.screenWidthDp >= 600) 2 else 1
            for (group in saved.chunked(columns)) {
                val row = LinearLayout(this@MainActivity).apply { orientation = LinearLayout.HORIZONTAL }
                for (host in group) row.addView(hostCard(host), LinearLayout.LayoutParams(0, WRAP_CONTENT, 1f).apply {
                    setMargins(0, 0, if (columns == 2 && host == group.first()) dp(12) else 0, dp(12))
                })
                if (columns == 2 && group.size == 1) row.addView(View(this@MainActivity), LinearLayout.LayoutParams(0, 0, 1f))
                addView(row, wide())
            }
        }
        val add = primary("+  Add host") { showHostForm() }.apply {
            contentDescription = "add host"
            textSize = 13f
            setPadding(dp(16), 0, dp(16), 0)
        }
        show(homePage("Hosts", "${saved.size} saved", body, add))
    }

    private fun showHostForm() {
        homeDetails = true
        val address = field("address", "", "gpu-box.example")
        val port = field("port", "22", "22").apply { inputType = InputType.TYPE_CLASS_NUMBER }
        val user = field("user", lastMachine()?.user ?: "", "user")
        fun destination(): Machine? {
            val host = address.text.toString().trim()
            val account = user.text.toString().trim()
            val number = port.text.toString().toUShortOrNull()
            if (host.isBlank() || account.isBlank()) {
                say("it wants an address and a user")
                return null
            }
            if (number == null || number == 0.toUShort()) {
                say("port must be between 1 and 65535")
                return null
            }
            return Machine(host, number, account)
        }
        val body = column().apply {
            addView(label("Address"))
            addView(address)
            addView(label("Username"))
            addView(user)
            addView(label("Port"))
            addView(port)
            addView(primary("save host") {
                destination()?.let { remember(it); showMachine() }
            })
            addView(secondary("Connect now") {
                destination()?.let {
                    homeDetails = false
                    machine = it
                    remember(it)
                    listSessions(it)
                }
            })
        }
        show(homePage("Add host", "SSH connection", body, back = true))
    }

    private fun showDeviceKey() {
        homeDetails = true
        val body = column().apply {
            addView(note("Add this public key to the host's authorized_keys to allow access."))
            addView(deviceKey(), wide().apply { topMargin = dp(16) })
            addView(copyTheKey())
        }
        show(homePage("SSH key", "This device's public key", body, back = true))
    }

    private fun homePage(heading: String, detail: String, body: View, action: View? = null, back: Boolean = false): View {
        val top = LinearLayout(this).apply {
            orientation = LinearLayout.HORIZONTAL
            gravity = Gravity.CENTER_VERTICAL
            setPadding(dp(20), dp(8), dp(20), dp(8))
            addView(TextView(this@MainActivity).apply {
                text = if (back) "Back" else "manymux"
                textSize = 14f
                typeface = Typeface.create("sans-serif-medium", Typeface.NORMAL)
                setTextColor(colour(if (back) R.color.accent else R.color.dim))
                gravity = Gravity.CENTER_VERTICAL
                minHeight = dp(48)
                if (back) {
                    isClickable = true
                    setBackgroundResource(pressable())
                    setOnClickListener { showMachine() }
                }
            }, LinearLayout.LayoutParams(0, dp(48), 1f))
            if (!back) addView(TextView(this@MainActivity).apply {
                text = "SSH key"
                textSize = 13f
                setTextColor(colour(R.color.dim))
                gravity = Gravity.CENTER
                contentDescription = "show SSH public key"
                isClickable = true
                setPadding(dp(12), 0, 0, 0)
                setBackgroundResource(pressable())
                setOnClickListener { showDeviceKey() }
            }, LinearLayout.LayoutParams(WRAP_CONTENT, dp(48)))
        }
        val headingRow = LinearLayout(this).apply {
            orientation = LinearLayout.HORIZONTAL
            gravity = Gravity.CENTER_VERTICAL
            setPadding(dp(20), dp(8), dp(20), dp(24))
            addView(LinearLayout(this@MainActivity).apply {
                orientation = LinearLayout.VERTICAL
                addView(TextView(this@MainActivity).apply {
                    text = heading
                    textSize = 30f
                    typeface = Typeface.create("sans-serif-medium", Typeface.NORMAL)
                    setTextColor(colour(R.color.text))
                })
                addView(TextView(this@MainActivity).apply {
                    text = detail
                    textSize = 12f
                    setTextColor(colour(R.color.dim))
                    setPadding(0, dp(6), 0, 0)
                })
            }, LinearLayout.LayoutParams(0, WRAP_CONTENT, 1f))
            action?.let { addView(it, LinearLayout.LayoutParams(WRAP_CONTENT, dp(48))) }
        }
        return LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setBackgroundColor(colour(R.color.ground))
            addView(top, wide())
            addView(headingRow, wide())
            addView(scrolling(body), LinearLayout.LayoutParams(MATCH_PARENT, 0, 1f))
        }
    }

    private fun hostCard(host: Machine): View {
        val destination = "${host.user}@${host.address}" +
            if (host.port == 22.toUShort()) "" else ":${host.port}"
        val words = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(dp(16), dp(22), dp(4), dp(22))
            addView(TextView(this@MainActivity).apply {
                text = host.address
                textSize = 16f
                typeface = Typeface.create("sans-serif-medium", Typeface.NORMAL)
                setTextColor(colour(R.color.text))
                maxLines = 1
                ellipsize = TextUtils.TruncateAt.END
            })
            addView(TextView(this@MainActivity).apply {
                text = "${host.user}  ·  port ${host.port}"
                textSize = 12f
                setTextColor(colour(R.color.dim))
                setPadding(0, dp(7), 0, 0)
                maxLines = 1
                ellipsize = TextUtils.TruncateAt.END
            })

        }
        return LinearLayout(this).apply {
            orientation = LinearLayout.HORIZONTAL
            gravity = Gravity.CENTER_VERTICAL
            background = panel(R.color.raised, dp(16))
            foreground = resources.getDrawable(pressable(), theme)
            contentDescription = "connect $destination"
            isClickable = true
            setOnClickListener {
                machine = host
                remember(host)
                listSessions(host)
            }
            addView(TextView(this@MainActivity).apply {
                text = ">_"
                textSize = 17f
                typeface = Typeface.MONOSPACE
                gravity = Gravity.CENTER
                setTextColor(colour(R.color.accent))
                background = panel(R.color.accentSurface, dp(12))
                importantForAccessibility = View.IMPORTANT_FOR_ACCESSIBILITY_NO
            }, LinearLayout.LayoutParams(dp(44), dp(44)).apply { leftMargin = dp(16) })
            addView(words, LinearLayout.LayoutParams(0, WRAP_CONTENT, 1f))
            addView(TextView(this@MainActivity).apply {
                text = "⋮"
                textSize = 22f
                gravity = Gravity.CENTER
                setTextColor(colour(R.color.dim))
                contentDescription = "options for $destination"
                isClickable = true
                setBackgroundResource(pressable())
                setOnClickListener { under ->
                    PopupMenu(this@MainActivity, under).apply {
                        menu.add("Remove host").setOnMenuItemClickListener {
                            hosts.remove(host)
                            showMachine()
                            true
                        }
                        show()
                    }
                }
            }, LinearLayout.LayoutParams(dp(48), dp(64)))
        }
    }

    // ---- what is running -----------------------------------------------

    /** The main screen: everything running on that machine. */
    private fun listSessions(machine: Machine) {
        hideKeyboard()
        answered = null
        show(overview(machine, column().apply { addView(note("reaching ${machine.address}")) }))

        loadWall(machine, surface)
    }

    private fun loadWall(machine: Machine, expected: Long) {
        elsewhere.execute {
            val answer = runCatching { phone.wall(machine) }
            here.post {
                if (isFinishing || gone || surface != expected) return@post
                val previous = (findViewById<android.view.ViewGroup>(android.R.id.content)
                    .getChildAt(0) as? LinearLayout)?.getChildAt(1) as? ScrollView
                val position = previous?.scrollY ?: 0
                val body = answer.fold(
                    onSuccess = { wall ->
                        seen = wall.running
                        answered = wall
                        sessions(machine, wall)
                    },
                    onFailure = { why ->
                        answered?.let { sessions(machine, it) } ?: trouble(machine, why)
                    },
                )
                val overview = overview(machine, body)
                show(overview)
                (overview as LinearLayout).getChildAt(1).let { scroll ->
                    scroll.post { scroll.scrollTo(0, position) }
                }
                scheduleWallRefresh(machine)
            }
        }
    }

    private fun scheduleWallRefresh(machine: Machine) {
        val current = surface
        wallRefresh = Runnable {
            if (surface == current && !gone && hasWindowFocus()) loadWall(machine, current)
        }.also { here.postDelayed(it, 2000) }
    }

    /**
     * Draw the same answer the other way, without asking the machine again.
     *
     * There is nothing to press before an answer has landed, so the button is
     * not on the bar then and this cannot be reached with [answered] empty.
     */
    private fun flipLayout(machine: Machine) {
        // Nothing to draw is nothing to remember either, or the setting and
        // the screen would part company over a press that did nothing.
        val wall = answered ?: return
        tiles = !tiles
        remembered.edit().putBoolean("tiles", tiles).apply()
        show(overview(machine, sessions(machine, wall)))
        scheduleWallRefresh(machine)
    }

    /** The screen the list sits in: a bar with the machine and a `+`. */
    private fun overview(machine: Machine, body: View): View {
        val name = TextView(this).apply {
            text = machine.address
            typeface = Typeface.MONOSPACE
            textSize = 15f
            setTextColor(colour(R.color.text))
        }
        val who = TextView(this).apply {
            text = machine.user
            textSize = 12f
            setTextColor(colour(R.color.dim))
        }
        val words = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            addView(name)
            addView(who)
            isClickable = true
            contentDescription = "saved hosts"
            setBackgroundResource(pressable())
            setOnClickListener { showMachine() }
        }
        val add = TextView(this).apply {
            text = "+"
            textSize = 22f
            gravity = Gravity.CENTER
            setTextColor(colour(R.color.ground))
            background = panel(R.color.accent, round = dp(8))
            setPadding(dp(14), dp(2), dp(14), dp(6))
            contentDescription = "new session, or another machine"
            isClickable = true
            setOnClickListener { view -> offerToAdd(view, machine) }
        }
        val bar = LinearLayout(this).apply {
            orientation = LinearLayout.HORIZONTAL
            gravity = Gravity.CENTER_VERTICAL
            setBackgroundColor(colour(R.color.panel))
            setPadding(dp(20), dp(16), dp(16), dp(16))
            addView(words, LinearLayout.LayoutParams(0, WRAP_CONTENT).apply { weight = 1f })
            howToDraw(machine)?.let { addView(it) }
            addView(add)
        }
        return LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            // The bar's colour rather than the ground's, because the root is
            // what carries the padding the system bars ask for: painted the
            // ground colour, the strip behind the clock read as a gap above
            // the bar instead of as the top of it.
            setBackgroundColor(colour(R.color.panel))
            addView(bar, LinearLayout.LayoutParams(MATCH_PARENT, WRAP_CONTENT))
            addView(
                scrolling(body),
                LinearLayout.LayoutParams(MATCH_PARENT, 0).apply { weight = 1f },
            )
        }
    }

    /**
     * The button for the other way of drawing them, where there is one.
     *
     * Nothing while a machine is still being reached, and nothing for one that
     * cannot be peeked: a switch with one destination is a control that lies
     * about what the app can do, and the list such a machine gets is the only
     * thing it has to show. It carries the glyph of what pressing it gives
     * rather than of what is on the screen, which is what every other view
     * switcher on the platform does and what the screen underneath already
     * says.
     */
    private fun howToDraw(machine: Machine): View? {
        if (answered?.previews != true) return null
        return TextView(this).apply {
            text = if (tiles) "☰" else "▦"
            textSize = 17f
            gravity = Gravity.CENTER
            setTextColor(colour(R.color.text))
            setBackgroundResource(pressable())
            setPadding(dp(12), dp(4), dp(16), dp(6))
            contentDescription = if (tiles) "show them as a list" else "show them as screens"
            isClickable = true
            setOnClickListener { flipLayout(machine) }
        }
    }

    /** What the `+` offers: another session here, or another machine. */
    private fun offerToAdd(under: View, machine: Machine) {
        PopupMenu(this, under).apply {
            menu.add("new session here").setOnMenuItemClickListener {
                start(machine)
                true
            }
            menu.add("another machine").setOnMenuItemClickListener {
                showMachine()
                true
            }
            show()
        }
    }

    /**
     * The wall: one tile per session, showing what is on its screen.
     *
     * Two across rather than a list of names, because a name is what somebody
     * called a session weeks ago and the screen is what it is doing now. A
     * machine with `build`, `deploy` and three shells on it is five words that
     * say the same amount as each other and a wall that does not.
     *
     * A machine too old to be peeked is drawn as a list of names instead, and
     * is not offered the choice. That is why [Wall.previews] is answered rather
     * than left to be guessed from empty screens: a tile with nothing in it
     * would otherwise read as a session sitting at a blank prompt, and there is
     * no telling the two apart from here.
     */
    private fun sessions(machine: Machine, wall: Wall): View =
        LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(dp(8), dp(8), dp(8), dp(24))
            if (wall.running.isEmpty()) {
                addView(note("Nothing is running there yet. The + starts a session."))
                return@apply
            }
            if (!wall.previews || !tiles) {
                if (!wall.previews) {
                    addView(note("That machine is too old to show what is on the screens."))
                }
                for (session in wall.running) {
                    addView(row(machine, session), between())
                }
                return@apply
            }
            val screens = wall.screens.associateBy { it.name }
            // Two at a time, since a row of a grid is what a linear layout has
            // and the alternative is a second layout manager for four lines of
            // arithmetic. An odd session out gets a blank beside it rather than
            // a tile of twice the width, or the last row would read as the
            // important one.
            for (pair in wall.running.chunked(ACROSS)) {
                val strip = LinearLayout(this@MainActivity).apply {
                    orientation = LinearLayout.HORIZONTAL
                }
                for (session in pair) {
                    strip.addView(tile(machine, session, screens[session.name]), share())
                }
                repeat(ACROSS - pair.size) {
                    strip.addView(View(this@MainActivity), share())
                }
                addView(strip, wide())
            }
        }

    /** Room for one of [ACROSS] tiles in a strip, and the gap around it. */
    private fun share() = LinearLayout.LayoutParams(0, WRAP_CONTENT).apply {
        weight = 1f
        setMargins(dp(6), dp(6), dp(6), dp(6))
    }

    /** The gap between one row of a list and the next. */
    private fun between() = wide().apply { setMargins(dp(6), dp(3), dp(6), dp(3)) }

    /**
     * One session as a row: what it is called, and how long since it was used.
     *
     * The same two words the tile carries under its screen, on the line the
     * screen would have been. Which is the point of the list rather than a
     * shortcoming of it: what a name is worth does not change with how much
     * room is spent on it, so a machine with twenty sessions on it is twenty
     * lines you can read in one look instead of ten screenfuls of pictures.
     */
    private fun row(machine: Machine, session: Running): View {
        val name = TextView(this).apply {
            text = session.name
            typeface = Typeface.MONOSPACE
            textSize = 15f
            isSingleLine = true
            setTextColor(colour(R.color.text))
        }
        return LinearLayout(this).apply {
            orientation = LinearLayout.HORIZONTAL
            gravity = Gravity.CENTER_VERTICAL
            isClickable = true
            background = panel(R.color.raised, round = dp(10))
            foreground = resources.getDrawable(pressable(), theme)
            setPadding(dp(14), dp(14), dp(14), dp(14))
            addView(name, LinearLayout.LayoutParams(0, WRAP_CONTENT).apply { weight = 1f })
            addView(idle(session))
            setOnClickListener { open(machine, session.name) }
        }
    }

    /**
     * How long since anything was typed there, and who is already in it.
     *
     * A dot for somebody else attached, the way the mark row draws one: two
     * people typing into one session is worth knowing before you start rather
     * than after. Shared by both ways of drawing a session, so the tile and
     * the row cannot end up saying different amounts about the same thing.
     */
    private fun idle(session: Running) = TextView(this).apply {
        text = if (session.attached > 0u) "● ${ago(session.idle)}" else ago(session.idle)
        textSize = 11f
        gravity = Gravity.END
        setTextColor(if (session.attached > 0u) colour(R.color.accent) else colour(R.color.hint))
    }

    /**
     * One session as a square: its screen, and underneath it what it is called.
     *
     * The name goes under the picture rather than over it. Written across the
     * top of the screen it is a caption on something that already has text all
     * over it, and the eye has to find the caption before it can use it; below
     * the tile every name in the wall is on the same line and they read as a
     * column.
     */
    private fun tile(machine: Machine, session: Running, screen: Preview?): View {
        val glass = SnapshotView(this).apply {
            preview = screen
            layoutParams = LinearLayout.LayoutParams(MATCH_PARENT, dp(TALL))
        }
        val name = TextView(this).apply {
            text = session.name
            typeface = Typeface.MONOSPACE
            textSize = 13f
            isSingleLine = true
            setTextColor(colour(R.color.text))
        }
        val under = LinearLayout(this).apply {
            orientation = LinearLayout.HORIZONTAL
            gravity = Gravity.CENTER_VERTICAL
            setPadding(dp(8), dp(7), dp(8), dp(8))
            addView(name, LinearLayout.LayoutParams(0, WRAP_CONTENT).apply { weight = 1f })
            addView(idle(session))
        }
        return LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            isClickable = true
            background = panel(R.color.raised, round = dp(10))
            foreground = resources.getDrawable(pressable(), theme)
            clipToOutline = true
            addView(glass)
            addView(under, wide())
            setOnClickListener { open(machine, session.name) }
        }
    }

    /**
     * A machine that would not answer, and whatever can be done about it.
     *
     * Which failure it was is read off the type rather than out of the words
     * (`ffi::Trouble`). Sniffing the sentence, as this did, means a message
     * reworded on the Rust side quietly takes the button that fixes it away,
     * and nothing anywhere fails when that happens.
     */
    private fun trouble(machine: Machine, why: Throwable): View = column().apply {
        addView(
            TextView(this@MainActivity).apply {
                text = why.message ?: "could not reach it"
                textSize = 14f
                setTextColor(colour(R.color.warn))
                setPadding(dp(14), dp(14), dp(14), dp(14))
                background = panel(R.color.raised)
            },
            wide(),
        )
        when (why) {
            // The account has never been given this device's key, which is
            // where every machine starts. Saying "add it to `authorized_keys`"
            // and then keeping the key itself behind a form on another screen
            // was a dead end: that line is in this app and in no other place a
            // phone can reach, so it belongs next to the sentence asking for
            // it.
            is Trouble.Refused -> {
                addView(label("This device's key"))
                addView(
                    note("Add this line to ${machine.user}'s authorized_keys on that machine."),
                )
                addView(deviceKey(), wide().apply { topMargin = dp(6) })
                addView(copyTheKey())
            }
            // Either a machine that was reinstalled or somebody in the middle,
            // and only the person reading it can say which. Saying so and
            // offering nothing was a dead end too.
            is Trouble.HostKey -> addView(
                secondary("it was reinstalled: forget the old key") {
                    phone.forget(machine)
                    listSessions(machine)
                },
            )
            else -> Unit
        }
        addView(primary("try again") { listSessions(machine) })
        addView(secondary("another machine") { showMachine() })
    }

    /** The line to paste into a machine's `authorized_keys`. */
    private fun deviceKey() = TextView(this).apply {
        text = phone.authorizedLine()
        typeface = Typeface.MONOSPACE
        textSize = 11f
        setTextColor(colour(R.color.dim))
        setPadding(dp(14), dp(12), dp(14), dp(12))
        background = panel(R.color.raised)
        // A key is pasted somewhere else, and getting it there off a phone
        // means either the button below or selecting it by hand.
        setTextIsSelectable(true)
    }

    private fun copyTheKey() = secondary("copy the key") {
        val clipboard = getSystemService(Context.CLIPBOARD_SERVICE) as ClipboardManager
        clipboard.setPrimaryClip(ClipData.newPlainText("manymux", phone.authorizedLine()))
        say("copied")
    }

    /** How long since anything was typed, in the one unit worth reading. */
    private fun ago(seconds: ULong): String {
        val s = seconds.toLong()
        return when {
            s < 60 -> "now"
            s < 3600 -> "${s / 60}m"
            s < 86400 -> "${s / 3600}h"
            else -> "${s / 86400}d"
        }
    }

    /** A login shell on that machine, opened as soon as it has a name. */
    private fun start(machine: Machine) {
        say("starting one")
        surface += 1
        wallRefresh?.let { here.removeCallbacks(it) }
        wallRefresh = null
        val expected = surface
        elsewhere.execute {
            // A size to start it at. The real one is sent the moment the view
            // knows its own, which is a resize the session has not printed
            // anything into yet.
            val answer = runCatching { phone.startOn(machine, Grid(80u, 24u)) }
            here.post {
                if (isFinishing || gone || surface != expected) return@post
                answer
                    .onSuccess { name -> open(machine, name) }
                    .onFailure { why ->
                        say(why.message ?: "could not start one")
                        scheduleWallRefresh(machine)
                    }
            }
        }
    }

    // ---- the session ----------------------------------------------------

    private fun open(machine: Machine, name: String) {
        // Whatever was open before this, if this is a switch rather than an
        // arrival: the tick holds the bar of a screen that is about to be
        // replaced, and a second attach left beside the first is two of them.
        letGo()

        val bar = TextView(this).apply {
            text = "${machine.address}/$name"
            typeface = Typeface.MONOSPACE
            setPadding(dp(2), dp(10), dp(14), dp(10))
            setTextColor(colour(R.color.text))
            textSize = 12f
        }
        // The way to the session next door, where a desktop has `Ctrl-] tab`.
        // Top left rather than beside the keys, because it is about where you
        // are and not about what you are typing, and because a hand holding a
        // phone one-handed is a hand that can reach a corner.
        val others = TextView(this).apply {
            text = "☰"
            textSize = 15f
            gravity = Gravity.CENTER
            setTextColor(colour(R.color.text))
            setBackgroundResource(pressable())
            setPadding(dp(16), dp(10), dp(12), dp(10))
            contentDescription = "the other sessions on this machine"
            isClickable = true
            setOnClickListener { under -> offerToSwitch(under, machine, name) }
        }
        val top = LinearLayout(this).apply {
            orientation = LinearLayout.HORIZONTAL
            gravity = Gravity.CENTER_VERTICAL
            setBackgroundColor(colour(R.color.panel))
            addView(others)
            addView(bar, LinearLayout.LayoutParams(0, WRAP_CONTENT).apply { weight = 1f })
        }

        val screen = TerminalView(this)
        terminal = screen
        screen.onCannotScroll = { say("$name is on a machine too old to scroll back") }

        val layout = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            // The bar above and the keys below are both this colour, so the
            // strips the system bars leave at the top and bottom continue them.
            setBackgroundColor(colour(R.color.panel))
            addView(top, LinearLayout.LayoutParams(MATCH_PARENT, WRAP_CONTENT))
            addView(
                screen,
                LinearLayout.LayoutParams(MATCH_PARENT, 0).apply { weight = 1f },
            )
            addView(extraKeys(screen), LinearLayout.LayoutParams(MATCH_PARENT, WRAP_CONTENT))
        }
        show(layout)

        // Attaching needs a grid to ask for, and the view only knows its size
        // once it has been laid out. So the attach waits for the first one.
        screen.onGrid = { grid ->
            if (attach == null) {
                val attached = phone.attach(machine, name, grid)
                attach = attached
                screen.attach = attached
                watch(attached, bar, machine, name)
            }
        }

        screen.openKeyboard()

        // And ask what else is running while nobody is waiting on the answer,
        // so the button above opens on this machine as it is rather than as it
        // was when the list was last drawn.
        refresh(machine)
    }

    /**
     * The other sessions on this machine, and a way straight into one.
     *
     * Opened from what is already known rather than from a fresh listing,
     * because the point of it is that it costs nothing: a switch that waited
     * for ssh is the trip through the list it exists to save. What lands under
     * the finger is therefore at most one listing old, which is what [refresh]
     * is for.
     */
    private fun offerToSwitch(under: View, machine: Machine, current: String) {
        // The keyboard goes first. A menu opened over one is a menu with the
        // bottom of it behind the keys, and on a short phone that is most of
        // the list; the gesture is about where you are going and not about
        // typing, so there is nothing it is in the way of.
        terminal?.closeKeyboard()

        // Asked for here as well as on the way in, so a session sat in for an
        // hour has a current list behind the second press if not the first.
        refresh(machine)

        val menu = PopupMenu(this, under)
        for (session in seen) {
            // The name and nothing else. A title is the last thing the program
            // set and a command is what it was started with, so a row carrying
            // one is a row of a length nobody chose, and what tells two rows
            // apart here is the one thing that is short and is a name.
            //
            // The one you are in is shown and not offered: a list with it
            // missing is a list whose rows move under a hand that has learnt
            // where they are, and picking it would tear the attach down to
            // build the same one again.
            val already = session.name == current
            menu.menu.add(if (already) "● ${session.name}" else session.name).apply {
                isEnabled = !already
                setOnMenuItemClickListener {
                    open(machine, session.name)
                    true
                }
            }
        }
        if (seen.isEmpty()) {
            menu.menu.add("nothing else answered").isEnabled = false
        }
        menu.show()
    }

    /**
     * Ask a machine what is running, for whoever looks next rather than now.
     *
     * At most one at a time, which is what [asking] is for: both ways in fire
     * on a switch, the executor is one thread, and a listing over a link that
     * has gone slow would leave the second queued behind the first for as long
     * as ssh takes to give up. Dropping the second costs nothing, since the
     * one already out is asking the same question.
     */
    private fun refresh(machine: Machine) {
        if (asking) return
        asking = true
        val expected = surface
        elsewhere.execute {
            val answer = runCatching { phone.runningOn(machine) }
            here.post {
                asking = false
                if (surface != expected) return@post
                if (isFinishing || gone) return@post
                // A failure says nothing: nobody asked for this, and the
                // session on the screen is the thing being used. The copy that
                // is already held stays, since a stale list beats no list.
                answer.onSuccess { seen = it }
            }
        }
    }

    /** The bar says what the mark row says: where you are, and how it is going. */
    private fun watch(attached: Attach, bar: TextView, machine: Machine, name: String) {
        val where = "${machine.address}/$name"
        val tick = object : Runnable {
            override fun run() {
                bar.text = when (val state = attached.state()) {
                    is State.Reaching -> "$where  ·  reaching"
                    is State.Attached -> where
                    // The screen above stays exactly as the session last
                    // painted it. Only this line changes, because a client
                    // that repainted or went back to a list would throw away
                    // the session, which is still running.
                    is State.Waiting -> "$where  ·  reconnecting (${state.tries})"
                    is State.Ended -> "$where  ·  ended ${state.status}"
                    is State.Detached -> "$where  ·  detached"
                    is State.Failed -> state.why
                }
                bar.setTextColor(
                    when (attached.state()) {
                        is State.Attached -> colour(R.color.text)
                        is State.Failed -> colour(R.color.warn)
                        is State.Waiting -> colour(R.color.warn)
                        else -> colour(R.color.dim)
                    },
                )
                here.postDelayed(this, 500)
            }
        }
        ticking = tick
        here.post(tick)
    }

    /**
     * Stop watching, and let go of the far end.
     *
     * All three parts matter. The tick reposts itself forever and holds the bar,
     * which holds the activity; the `Attach` is a handle to an object on the
     * other side of the boundary that nothing else will release; and leaving
     * either behind means a second session opened later has two of them.
     *
     * What this does *not* let go of is the ssh connection, which belongs to
     * the `Phone` and outlives every attach made over it.
     */
    private fun letGo() {
        ticking?.let { here.removeCallbacks(it) }
        ticking = null
        terminal?.attach = null
        terminal = null
        attach?.detach()
        attach?.close()
        attach = null
    }

    /** The keys a phone keyboard has not got. */
    private fun extraKeys(screen: TerminalView): View {
        val keys = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setBackgroundColor(colour(R.color.panel))
            setPadding(dp(6), dp(6), dp(6), dp(6))
        }
        fun row(): LinearLayout = LinearLayout(this).also {
            it.orientation = LinearLayout.HORIZONTAL
            keys.addView(it, wide())
        }
        fun space(row: LinearLayout) {
            row.addView(View(this), LinearLayout.LayoutParams(0, dp(48), 1f).apply { setMargins(dp(2), dp(2), dp(2), dp(2)) })
        }
        fun key(row: LinearLayout, label: String, description: String = label, press: (TextView) -> Unit): TextView {
            val button = TextView(this).apply {
                text = label
                contentDescription = description
                textSize = if (description in listOf("up", "down", "left", "right")) 21f else 13f
                gravity = Gravity.CENTER
                setTextColor(colour(R.color.text))
                isClickable = true
                background = buttonBackground(R.color.raised, selectable = true)
            }
            button.setOnClickListener { press(button) }
            row.addView(button, LinearLayout.LayoutParams(0, dp(48), 1f).apply { setMargins(dp(2), dp(2), dp(2), dp(2)) })
            return button
        }
        val top = row()
        key(top, "esc") { screen.send(byteArrayOf(0x1b)) }
        key(top, "tab") { screen.send(byteArrayOf(0x09)) }
        // Held rather than pressed with something: the next character typed is
        // the chord, which is the only way a soft keyboard can spell one. It
        // says so by staying lit, since a modifier you cannot see the state of
        // is one you press twice.
        val ctrl = key(top, "ctrl") { screen.control = !screen.control }
        fun showControl(held: Boolean) {
            ctrl.isSelected = held
            ctrl.setTextColor(if (held) colour(R.color.accent) else colour(R.color.text))
        }
        screen.onControlChanged = ::showControl
        showControl(screen.control)
        space(top)
        key(top, "↑", "up") { screen.cursorKey(Direction.UP) }
        space(top)
        val bottom = row()
        key(bottom, "paste") {
            val clipboard = getSystemService(Context.CLIPBOARD_SERVICE) as ClipboardManager
            val text = clipboard.primaryClip?.getItemAt(0)?.coerceToText(this)?.toString()
            if (!text.isNullOrEmpty()) screen.paste(text)
        }
        key(bottom, "⌨", "keyboard") { screen.openKeyboard() }
        space(bottom)
        key(bottom, "←", "left") { screen.cursorKey(Direction.LEFT) }
        key(bottom, "↓", "down") { screen.cursorKey(Direction.DOWN) }
        key(bottom, "→", "right") { screen.cursorKey(Direction.RIGHT) }
        return keys
    }

    // ---- the small stuff ----------------------------------------------

    /**
     * Put a screen up, inside whatever the system bars have left.
     *
     * Every screen goes through here rather than `setContentView`, because an
     * app targeting SDK 36 is laid out edge to edge whether it asked to be or
     * not: SDK 35 had an opt-out flag and this one does not, so a screen put
     * up without reading the insets draws its top row underneath the clock.
     * Which is what it did: the machine's name sat behind the status bar and
     * the `+` behind the battery.
     *
     * The padding goes on the root and the root is painted the same colour as
     * the bar, so the strip behind the status bar reads as part of the bar
     * rather than as a gap above it. The keyboard is in the same set on
     * purpose: `adjustResize` is what used to lift the extra keys clear of it,
     * and a window this app lays out itself is no longer resized for us, which
     * [onCreate] now says outright rather than leaving to the platform.
     */
    private fun show(view: View) {
        surface += 1
        wallRefresh?.let { here.removeCallbacks(it) }
        wallRefresh = null
        // Whatever was moving was moving on the screen this replaces, and its
        // callback goes with it: a view taken out of the hierarchy part way
        // through an animation is never told the animation ended, so the flag
        // that animation set stays set. Left set, the listener below declines
        // to pad the screen that has just gone up, and it is the only thing
        // that would have: the new screen's own first dispatch is the one
        // arriving here, and there is no animation on this view for it to
        // fight. Leaving a session puts the keyboard away, which is an
        // animation in flight at exactly the moment the list goes up.
        lifting = false
        view.setOnApplyWindowInsetsListener { at, insets ->
            if (!lifting) fit(at, insets)
            insets
        }
        follow(view)
        setContentView(view)
        // And asks for them rather than waiting to be told. Insets are
        // dispatched when they change, and a screen put up in the middle of a
        // run is a new view in a window whose insets have not changed at all:
        // whatever the last screen was told, this one has been told nothing,
        // and sits unpadded until something moves. The first screen of a run is
        // the one that gets a dispatch for nothing, which is what made this
        // look done.
        view.requestApplyInsets()
    }

    /**
     * Follow the keyboard as it moves, rather than waiting to be told where it
     * stopped.
     *
     * A screen padded only from [show]'s listener is a screen that depends on
     * one dispatch landing. It does not always: a keyboard asked for in the
     * same breath as the screen going up races the traversal that putting the
     * screen up already scheduled, and the session opened under a keyboard
     * that covered its last rows until it was put away and brought back, which
     * is a second change and a second dispatch. Read off the animation instead,
     * the padding is taken from every frame the keyboard moves through and
     * again from the window itself once it has stopped, so a missed dispatch is
     * one frame rather than the rest of the attach.
     *
     * [lifting] is what keeps the two from fighting. The framework calls
     * `onApplyWindowInsets` with the *end* of the animation before it starts
     * running it, and padding for that straight away would put the screen where
     * the keyboard is going and then animate it back down from nothing.
     */
    private fun follow(view: View) {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.R) return
        view.setWindowInsetsAnimationCallback(
            object : WindowInsetsAnimation.Callback(DISPATCH_MODE_CONTINUE_ON_SUBTREE) {
                override fun onPrepare(animation: WindowInsetsAnimation) {
                    if (animation.typeMask and WindowInsets.Type.ime() != 0) lifting = true
                }

                override fun onProgress(
                    insets: WindowInsets,
                    running: MutableList<WindowInsetsAnimation>,
                ): WindowInsets {
                    fit(view, insets)
                    return insets
                }

                override fun onEnd(animation: WindowInsetsAnimation) {
                    lifting = false
                    // Where it actually ended, asked of the window rather than
                    // taken from the last frame: an animation can be cut short
                    // or replaced by another, and the frame before that is not
                    // where the keyboard is now.
                    view.rootWindowInsets?.let { fit(view, it) }
                }
            },
        )
    }

    /** Put a screen inside what the bars, the cutout and the keyboard leave. */
    private fun fit(at: View, insets: WindowInsets) {
        val edges = edges(insets)
        at.setPadding(edges.left, edges.top, edges.right, edges.bottom)
    }

    /** What the bars, the cutout and the keyboard are covering. */
    private fun edges(insets: WindowInsets): Rect =
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
            val covered = insets.getInsets(
                WindowInsets.Type.systemBars() or
                    WindowInsets.Type.displayCutout() or
                    WindowInsets.Type.ime(),
            )
            Rect(covered.left, covered.top, covered.right, covered.bottom)
        } else {
            // Below API 30 there is one set of insets and the window is still
            // resized for the keyboard, so this is the whole of it.
            @Suppress("DEPRECATION")
            Rect(
                insets.systemWindowInsetLeft,
                insets.systemWindowInsetTop,
                insets.systemWindowInsetRight,
                insets.systemWindowInsetBottom,
            )
        }

    /** Density-independent pixels, which is what every size here is written in. */
    private fun dp(value: Int): Int = (value * resources.displayMetrics.density).toInt()

    private fun colour(id: Int): Int = resources.getColor(id, theme)

    /** A filled, optionally rounded background. */
    private fun panel(id: Int, round: Int = dp(6)): GradientDrawable = GradientDrawable().apply {
        setColor(colour(id))
        cornerRadius = round.toFloat()
    }

    private fun buttonBackground(fill: Int, selectable: Boolean = false): RippleDrawable {
        fun shape(colour: Int) = panel(colour, dp(12)).apply { setStroke(dp(1), this@MainActivity.colour(R.color.line)) }
        val states = StateListDrawable().apply {
            if (selectable) addState(intArrayOf(android.R.attr.state_selected), shape(R.color.accentSurface))
            addState(intArrayOf(), shape(fill))
        }
        return RippleDrawable(ColorStateList.valueOf(colour(R.color.line)), states, panel(R.color.text, dp(12)))
    }

    /** The platform's own press feedback, so a row looks like something to tap. */
    private fun pressable(): Int {
        val out = android.util.TypedValue()
        theme.resolveAttribute(android.R.attr.selectableItemBackground, out, true)
        return out.resourceId
    }

    private fun scrolling(body: View) = ScrollView(this).apply {
        setBackgroundColor(colour(R.color.ground))
        isFillViewport = true
        addView(body, LinearLayout.LayoutParams(MATCH_PARENT, WRAP_CONTENT))
    }

    private fun column() = LinearLayout(this).apply {
        orientation = LinearLayout.VERTICAL
        setPadding(dp(20), dp(20), dp(20), dp(32))
    }

    private fun wide() = LinearLayout.LayoutParams(MATCH_PARENT, WRAP_CONTENT)

    private fun title(what: String) = TextView(this).apply {
        text = what
        textSize = 32f
        typeface = Typeface.create("sans-serif-medium", Typeface.NORMAL)
        setTextColor(colour(R.color.text))
        setPadding(0, dp(8), 0, dp(4))
    }

    /** A small uppercase heading, which is what separates the sections. */
    private fun label(what: String) = TextView(this).apply {
        text = what.uppercase()
        textSize = 11f
        letterSpacing = 0.14f
        setTextColor(colour(R.color.accent))
        setPadding(0, dp(20), 0, dp(10))
    }

    private fun note(what: String) = TextView(this).apply {
        text = what
        textSize = 13f
        setTextColor(colour(R.color.dim))
        setPadding(0, dp(8), 0, dp(12))
    }

    private fun field(what: String, filled: String, example: String) = EditText(this).apply {
        hint = example
        contentDescription = what
        setText(filled)
        setSingleLine()
        textSize = 15f
        setTextColor(colour(R.color.text))
        setHintTextColor(colour(R.color.hint))
        background = panel(R.color.raised, dp(12))
        setPadding(dp(14), dp(14), dp(14), dp(14))
        layoutParams = wide().apply { bottomMargin = dp(8) }
    }

    /** The one button on a screen that says what the screen is for. */
    private fun primary(what: String, press: () -> Unit) = TextView(this).apply {
        text = what
        textSize = 15f
        gravity = Gravity.CENTER
        setTextColor(colour(R.color.ground))
        background = buttonBackground(R.color.accent)
        minHeight = dp(48)
        setPadding(dp(16), dp(15), dp(16), dp(15))
        isClickable = true
        setOnClickListener { press() }
        layoutParams = wide().apply { topMargin = dp(10) }
    }

    private fun secondary(what: String, press: () -> Unit) = TextView(this).apply {
        text = what
        textSize = 14f
        gravity = Gravity.CENTER
        setTextColor(colour(R.color.text))
        background = buttonBackground(R.color.ground)
        minHeight = dp(48)
        setPadding(dp(16), dp(13), dp(16), dp(13))
        isClickable = true
        setOnClickListener { press() }
        layoutParams = wide().apply { topMargin = dp(8) }
    }

    private fun hideKeyboard() {
        val token = currentFocus?.windowToken ?: return
        val keyboard = getSystemService(Context.INPUT_METHOD_SERVICE) as InputMethodManager
        keyboard.hideSoftInputFromWindow(token, 0)
    }

    private fun say(what: String) {
        Toast.makeText(this, what, Toast.LENGTH_SHORT).show()
    }

    private companion object {
        /** Tiles to a row. Two is what fits with the text under one legible. */
        const val ACROSS = 2

        /**
         * How tall a tile's screen is, in dp.
         *
         * Fixed rather than square, because a terminal is not: a screen is
         * about twice as wide as it is tall in pixels, and a square tile spends
         * the bottom half of itself on nothing. This is roughly that ratio at
         * half a phone's width.
         */
        const val TALL = 110
    }
}
