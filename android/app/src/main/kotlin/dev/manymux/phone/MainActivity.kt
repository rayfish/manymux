package dev.manymux.phone

import android.app.Activity
import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.content.SharedPreferences
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.text.InputType
import android.view.Gravity
import android.view.View
import android.view.ViewGroup.LayoutParams.MATCH_PARENT
import android.view.ViewGroup.LayoutParams.WRAP_CONTENT
import android.view.inputmethod.InputMethodManager
import android.widget.Button
import android.widget.EditText
import android.widget.LinearLayout
import android.widget.PopupMenu
import android.widget.ScrollView
import android.widget.TextView
import android.widget.Toast
import java.util.concurrent.Executors
import uniffi.manymux_android.Attach
import uniffi.manymux_android.Grid
import uniffi.manymux_android.Machine
import uniffi.manymux_android.Phone
import uniffi.manymux_android.Running
import uniffi.manymux_android.State

/**
 * The whole app: a machine, what is running on it, and one of those on the
 * screen.
 *
 * There is no mode key here, which is the idea the rest of it hangs on. On a
 * desktop `Ctrl-]` exists because a terminal has one keyboard and the session
 * wants every key of it; a phone has chrome the session does not own, so the
 * key goes straight through and the verbs behind it become things you press.
 *
 * The shape that follows from it is the platform's own: what is running is the
 * main screen, a session is a screen on top of it, and back is how you come
 * out. That is already the gesture people have, since a swipe in from the left
 * edge *is* back on any phone with gesture navigation, and it needs no drawer
 * and nothing to learn. What made it feel like a place you would rather not
 * leave was never the navigation, it was that leaving used to take the ssh
 * connection with it. It no longer does (`machine::Connections`), so the list
 * is the fast way between two sessions and there is no second surface that
 * would also be one.
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

    /** The bar's own clock, kept so it can be stopped. */
    private var ticking: Runnable? = null

    /** Whether everything has been let go of, so late work stays away. */
    private var gone = false

    private lateinit var remembered: SharedPreferences

    override fun onCreate(saved: Bundle?) {
        super.onCreate(saved)
        // App-private storage: the key this device is known by lives here and
        // nowhere else.
        phone = Phone.keptIn(filesDir.absolutePath)
        remembered = getSharedPreferences("manymux", MODE_PRIVATE)

        // Straight to what is running, if there is somewhere to ask. Opening on
        // a form asking for an address every time would be asking somebody to
        // type what the app already knows in order to see what it is for.
        val known = lastMachine()
        if (known == null) {
            showMachine()
        } else {
            machine = known
            listSessions(known)
        }
    }

    override fun onDestroy() {
        super.onDestroy()
        // The session goes on running on the machine. This only stops watching
        // it, which is the whole point of the thing.
        gone = true
        letGo()
        phone.close()
        elsewhere.shutdownNow()
    }

    // ---- the machine ---------------------------------------------------

    private fun lastMachine(): Machine? {
        val address = remembered.getString("address", "") ?: ""
        if (address.isBlank()) return null
        return Machine(
            address,
            remembered.getInt("port", 22).toUShort(),
            remembered.getString("user", "") ?: "",
        )
    }

    private fun remember(machine: Machine) {
        remembered.edit()
            .putString("address", machine.address)
            .putInt("port", machine.port.toInt())
            .putString("user", machine.user)
            .apply()
    }

    /** The form: which machine, and this device's key. */
    private fun showMachine() {
        val known = lastMachine()
        val layout = column()
        val address = field("address", known?.address ?: "")
        val port = field("port", (known?.port ?: 22u).toString()).apply {
            inputType = InputType.TYPE_CLASS_NUMBER
        }
        val user = field("user", known?.user ?: "")

        layout.addView(heading("Reach a machine"))
        layout.addView(address)
        layout.addView(port)
        layout.addView(user)
        layout.addView(
            Button(this).apply {
                text = "sessions"
                setOnClickListener {
                    val reaching = Machine(
                        address.text.toString().trim(),
                        (port.text.toString().toUShortOrNull() ?: 22u),
                        user.text.toString().trim(),
                    )
                    machine = reaching
                    remember(reaching)
                    listSessions(reaching)
                }
            },
        )

        layout.addView(heading("This device's key"))
        layout.addView(
            TextView(this).apply {
                text = phone.authorizedLine()
                textSize = 11f
                setPadding(0, 8, 0, 8)
            },
        )
        layout.addView(
            Button(this).apply {
                text = "copy the key"
                setOnClickListener {
                    val clipboard = getSystemService(Context.CLIPBOARD_SERVICE) as ClipboardManager
                    clipboard.setPrimaryClip(
                        ClipData.newPlainText("manymux", phone.authorizedLine()),
                    )
                    say("copied: put it in that account's authorized_keys")
                }
            },
        )

        setContentView(ScrollView(this).apply { addView(layout) })
    }

    // ---- what is running -----------------------------------------------

    /** The main screen: everything running on that machine. */
    private fun listSessions(machine: Machine) {
        setContentView(overview(machine, reaching(machine.address)))

        elsewhere.execute {
            val answer = runCatching { phone.runningOn(machine) }
            here.post {
                // The answer can arrive after somebody left: the executor
                // cannot interrupt a thread sitting in a call across the
                // boundary, so this lands on an activity that has already let
                // go of everything.
                if (isFinishing || gone) return@post
                val body = answer.fold(
                    onSuccess = { running -> sessions(machine, running) },
                    onFailure = { why -> trouble(machine, why.message ?: "could not reach it") },
                )
                setContentView(overview(machine, body))
            }
        }
    }

    /** The screen the list sits in: a bar with the machine and a `+`. */
    private fun overview(machine: Machine, body: View): View {
        val name = TextView(this).apply {
            text = machine.address
            textSize = 17f
            setPadding(0, 0, 0, 0)
        }
        val add = Button(this).apply {
            text = "+"
            textSize = 20f
            minWidth = 0
            setPadding(28, 0, 28, 0)
            contentDescription = "new session, or another machine"
            setOnClickListener { view -> offerToAdd(view, machine) }
        }
        val bar = LinearLayout(this).apply {
            orientation = LinearLayout.HORIZONTAL
            gravity = Gravity.CENTER_VERTICAL
            setPadding(32, 20, 20, 12)
            addView(name, LinearLayout.LayoutParams(0, WRAP_CONTENT).apply { weight = 1f })
            addView(add, LinearLayout.LayoutParams(WRAP_CONTENT, WRAP_CONTENT))
        }
        return LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            addView(bar, LinearLayout.LayoutParams(MATCH_PARENT, WRAP_CONTENT))
            addView(
                ScrollView(this@MainActivity).apply { addView(body) },
                LinearLayout.LayoutParams(MATCH_PARENT, 0).apply { weight = 1f },
            )
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

    private fun reaching(address: String) = column().apply {
        addView(note("reaching $address"))
    }

    private fun sessions(machine: Machine, running: List<Running>): View = column().apply {
        if (running.isEmpty()) {
            addView(note("nothing is running there yet. The + starts one."))
        }
        for (session in running) {
            addView(row(machine, session))
        }
    }

    private fun trouble(machine: Machine, said: String): View = column().apply {
        addView(note(said))
        // The one failure with something to press: a key that changed is
        // either a machine that was reinstalled or somebody in the middle, and
        // only the person reading it can say which. Saying so and offering
        // nothing was a dead end.
        if (said.contains("host key")) {
            addView(
                Button(this@MainActivity).apply {
                    text = "it was reinstalled: forget the old key"
                    setOnClickListener {
                        phone.forget(machine)
                        listSessions(machine)
                    }
                },
            )
        }
        addView(
            Button(this@MainActivity).apply {
                text = "try again"
                setOnClickListener { listSessions(machine) }
            },
        )
    }

    /** One session in the list: what it is called, what it is doing, how long ago. */
    private fun row(machine: Machine, session: Running): View {
        val what = if (session.title.isBlank()) session.command else session.title
        val name = TextView(this).apply {
            text = session.name
            textSize = 16f
        }
        val doing = TextView(this).apply {
            text = what
            textSize = 13f
            isSingleLine = true
        }
        val since = TextView(this).apply {
            // A dot for somebody else already in there, the way the mark row
            // draws one: two people typing into one session is worth knowing
            // before you start rather than after.
            text = if (session.attached > 0u) "● ${ago(session.idle)}" else ago(session.idle)
            textSize = 13f
            gravity = Gravity.END
        }
        val words = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            addView(name)
            addView(doing)
        }
        return LinearLayout(this).apply {
            orientation = LinearLayout.HORIZONTAL
            gravity = Gravity.CENTER_VERTICAL
            isClickable = true
            setPadding(24, 22, 24, 22)
            addView(words, LinearLayout.LayoutParams(0, WRAP_CONTENT).apply { weight = 1f })
            addView(since, LinearLayout.LayoutParams(WRAP_CONTENT, WRAP_CONTENT))
            setOnClickListener { open(machine, session.name) }
        }
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
        elsewhere.execute {
            // A size to start it at. The real one is sent the moment the view
            // knows its own, which is a resize the session has not printed
            // anything into yet.
            val answer = runCatching { phone.startOn(machine, Grid(80u, 24u)) }
            here.post {
                if (isFinishing || gone) return@post
                answer
                    .onSuccess { name -> open(machine, name) }
                    .onFailure { why -> say(why.message ?: "could not start one") }
            }
        }
    }

    // ---- the session ----------------------------------------------------

    private fun open(machine: Machine, name: String) {
        val bar = TextView(this).apply {
            text = "${machine.address}/$name"
            setPadding(16, 12, 16, 12)
            setBackgroundColor(0xFF22252A.toInt())
            setTextColor(Palette.TEXT)
            textSize = 13f
        }

        val screen = TerminalView(this)
        terminal = screen

        val layout = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setBackgroundColor(Palette.GROUND)
            addView(bar, LinearLayout.LayoutParams(MATCH_PARENT, WRAP_CONTENT))
            addView(
                screen,
                LinearLayout.LayoutParams(MATCH_PARENT, 0).apply { weight = 1f },
            )
            addView(extraKeys(screen), LinearLayout.LayoutParams(MATCH_PARENT, WRAP_CONTENT))
        }
        setContentView(layout)

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

        screen.requestFocus()
        (getSystemService(Context.INPUT_METHOD_SERVICE) as InputMethodManager)
            .showSoftInput(screen, InputMethodManager.SHOW_IMPLICIT)
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
            orientation = LinearLayout.HORIZONTAL
            setBackgroundColor(0xFF22252A.toInt())
        }
        fun key(label: String, press: () -> Unit) {
            keys.addView(
                Button(this).apply {
                    text = label
                    textSize = 12f
                    setPadding(0, 0, 0, 0)
                    setOnClickListener { press() }
                },
                LinearLayout.LayoutParams(0, WRAP_CONTENT).apply { weight = 1f },
            )
        }
        key("esc") { screen.send(byteArrayOf(0x1b)) }
        key("tab") { screen.send(byteArrayOf(0x09)) }
        // Held rather than pressed with something: the next character typed is
        // the chord, which is the only way a soft keyboard can spell one.
        key("ctrl") {
            screen.control = !screen.control
            say(if (screen.control) "ctrl: the next key" else "ctrl off")
        }
        key("↑") { screen.send("\u001b[A".toByteArray()) }
        key("↓") { screen.send("\u001b[B".toByteArray()) }
        key("←") { screen.send("\u001b[D".toByteArray()) }
        key("→") { screen.send("\u001b[C".toByteArray()) }
        key("paste") {
            val clipboard = getSystemService(Context.CLIPBOARD_SERVICE) as ClipboardManager
            val text = clipboard.primaryClip?.getItemAt(0)?.coerceToText(this)?.toString()
            if (!text.isNullOrEmpty()) screen.send(text.toByteArray())
        }
        return keys
    }

    /**
     * Back leaves the session running and goes back to what is running.
     *
     * This is also the swipe: on a phone with gesture navigation, dragging in
     * from the left edge *is* back, so the gesture people already have lands
     * here without the app claiming an edge or drawing a drawer.
     *
     * `onBackPressed` and not `onKeyDown`: from Android 15 an app that targets
     * it gets predictive back by default, and the platform stops dispatching
     * `KEYCODE_BACK` as a key at all. Read there, the one gesture that gets you
     * out of a session would instead close the app.
     */
    @Suppress("DEPRECATION", "MissingSuperCall")
    override fun onBackPressed() {
        if (attach == null) {
            @Suppress("DEPRECATION")
            super.onBackPressed()
            return
        }
        letGo()
        machine?.let { listSessions(it) } ?: showMachine()
    }

    // ---- the small stuff ----------------------------------------------

    private fun column() = LinearLayout(this).apply {
        orientation = LinearLayout.VERTICAL
        setPadding(8, 8, 8, 32)
    }

    private fun heading(what: String) = TextView(this).apply {
        text = what
        textSize = 18f
        setPadding(24, 16, 24, 8)
    }

    private fun note(what: String) = TextView(this).apply {
        text = what
        setPadding(24, 8, 24, 8)
    }

    private fun field(what: String, filled: String) = EditText(this).apply {
        hint = what
        setText(filled)
        setSingleLine()
        setPadding(24, 12, 24, 12)
    }

    private fun say(what: String) {
        Toast.makeText(this, what, Toast.LENGTH_SHORT).show()
    }
}
