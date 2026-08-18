package dev.manymux.phone

import android.app.Activity
import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.text.InputType
import android.view.Gravity
import android.view.KeyEvent
import android.view.View
import android.view.ViewGroup.LayoutParams.MATCH_PARENT
import android.view.ViewGroup.LayoutParams.WRAP_CONTENT
import android.view.inputmethod.InputMethodManager
import android.widget.Button
import android.widget.EditText
import android.widget.LinearLayout
import android.widget.ScrollView
import android.widget.TextView
import android.widget.Toast
import java.util.concurrent.Executors
import uniffi.manymux_android.Attach
import uniffi.manymux_android.Machine
import uniffi.manymux_android.Phone
import uniffi.manymux_android.Running
import uniffi.manymux_android.State

/**
 * The whole app: a machine, its sessions, and one of them on the screen.
 *
 * There is no mode key here, which is the idea the rest of it hangs on. On a
 * desktop `Ctrl-]` exists because a terminal has one keyboard and the session
 * wants every key of it; a phone has chrome the session does not own, so the
 * key goes straight through and the verbs behind it become things you press.
 * In this version that is the bar at the top and the back button; the drawer,
 * the swipes and the groups are the next ones.
 */
class MainActivity : Activity() {

    private lateinit var phone: Phone

    /** Anything that talks to a machine, off the thread drawing the screen. */
    private val elsewhere = Executors.newSingleThreadExecutor()
    private val here = Handler(Looper.getMainLooper())

    private var attach: Attach? = null
    private var terminal: TerminalView? = null

    /** The machine last connected to, kept so a session list can be reopened. */
    private var machine: Machine? = null

    override fun onCreate(saved: Bundle?) {
        super.onCreate(saved)
        // App-private storage: the key this device is known by lives here and
        // nowhere else.
        phone = Phone.keptIn(filesDir.absolutePath)
        showMachine()
    }

    override fun onDestroy() {
        super.onDestroy()
        // The session goes on running on the machine. This only stops watching
        // it, which is the whole point of the thing.
        attach?.detach()
        elsewhere.shutdownNow()
    }

    // ---- where to go --------------------------------------------------

    /** The first screen: which machine, and this device's key. */
    private fun showMachine() {
        val layout = column()
        val address = field("address", "")
        val port = field("port", "22").apply {
            inputType = InputType.TYPE_CLASS_NUMBER
        }
        val user = field("user", "")

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

    /** The second: what is running there. */
    private fun listSessions(machine: Machine) {
        val layout = column()
        layout.addView(heading("reaching ${machine.address}"))
        setContentView(ScrollView(this).apply { addView(layout) })

        elsewhere.execute {
            val answer = runCatching { phone.runningOn(machine) }
            here.post {
                val rows = column()
                answer
                    .onSuccess { sessions ->
                        rows.addView(heading(machine.address))
                        if (sessions.isEmpty()) {
                            rows.addView(note("nothing is running there"))
                        }
                        for (session in sessions) {
                            rows.addView(row(machine, session))
                        }
                    }
                    .onFailure { why ->
                        rows.addView(heading(machine.address))
                        rows.addView(note(why.message ?: "could not reach it"))
                    }
                rows.addView(
                    Button(this).apply {
                        text = "another machine"
                        setOnClickListener { showMachine() }
                    },
                )
                setContentView(ScrollView(this).apply { addView(rows) })
            }
        }
    }

    /** One session in the list. */
    private fun row(machine: Machine, session: Running): View = Button(this).apply {
        val what = if (session.title.isBlank()) session.command else session.title
        text = "${session.name}\n$what"
        gravity = Gravity.START
        setOnClickListener { open(machine, session.name) }
    }

    /** The third: the session itself. */
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
        here.post(tick)
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

    /** Back leaves the session running and goes back to the list. */
    override fun onKeyDown(code: Int, event: KeyEvent): Boolean {
        if (code == KeyEvent.KEYCODE_BACK && attach != null) {
            attach?.detach()
            attach = null
            terminal?.attach = null
            machine?.let { listSessions(it) } ?: showMachine()
            return true
        }
        return super.onKeyDown(code, event)
    }

    // ---- the small stuff ----------------------------------------------

    private fun column() = LinearLayout(this).apply {
        orientation = LinearLayout.VERTICAL
        setPadding(32, 32, 32, 32)
    }

    private fun heading(what: String) = TextView(this).apply {
        text = what
        textSize = 18f
        setPadding(0, 16, 0, 8)
    }

    private fun note(what: String) = TextView(this).apply {
        text = what
        setPadding(0, 8, 0, 8)
    }

    private fun field(what: String, filled: String) = EditText(this).apply {
        hint = what
        setText(filled)
        setSingleLine()
    }

    private fun say(what: String) {
        Toast.makeText(this, what, Toast.LENGTH_SHORT).show()
    }
}
