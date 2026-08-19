package dev.manymux.phone

import android.app.Activity
import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.content.SharedPreferences
import android.graphics.Color
import android.graphics.Rect
import android.graphics.Typeface
import android.graphics.drawable.GradientDrawable
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.text.InputType
import android.view.Gravity
import android.view.View
import android.view.ViewGroup.LayoutParams.MATCH_PARENT
import android.view.ViewGroup.LayoutParams.WRAP_CONTENT
import android.view.WindowInsets
import android.widget.EditText
import android.widget.LinearLayout
import android.widget.PopupMenu
import android.widget.ScrollView
import android.widget.TextView
import android.widget.Toast
import android.window.OnBackInvokedDispatcher
import java.util.concurrent.Executors
import uniffi.manymux_android.Attach
import uniffi.manymux_android.Grid
import uniffi.manymux_android.Machine
import uniffi.manymux_android.Phone
import uniffi.manymux_android.Running
import uniffi.manymux_android.State
import uniffi.manymux_android.Trouble

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

    /** The bar's own clock, kept so it can be stopped. */
    private var ticking: Runnable? = null

    /** Whether everything has been let go of, so late work stays away. */
    private var gone = false

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
        takeBack()

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
            finish()
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
        val address = field("address", known?.address ?: "", "gpu-box.example")
        val port = field("port", (known?.port ?: 22u).toString(), "22").apply {
            inputType = InputType.TYPE_CLASS_NUMBER
        }
        val user = field("user", known?.user ?: "", "who to be there")

        val layout = column().apply {
            addView(title("manymux"))
            addView(
                note(
                    "A terminal you can work in, on machines you reach over ssh. " +
                        "Sessions keep running when you leave.",
                ),
            )

            addView(label("Machine"))
            addView(address)
            addView(port)
            addView(user)
            addView(
                primary("see what is running") {
                    val reaching = Machine(
                        address.text.toString().trim(),
                        (port.text.toString().toUShortOrNull() ?: 22u),
                        user.text.toString().trim(),
                    )
                    if (reaching.address.isBlank() || reaching.user.isBlank()) {
                        say("it wants an address and a user")
                    } else {
                        machine = reaching
                        remember(reaching)
                        listSessions(reaching)
                    }
                },
            )

            addView(label("This device's key"))
            addView(
                note("That machine will not let this in until the line below is in the account's authorized_keys."),
            )
            addView(deviceKey(), wide().apply { topMargin = dp(6) })
            addView(copyTheKey())
            addView(
                TextView(this@MainActivity).apply {
                    text = version()
                    textSize = 11f
                    setTextColor(colour(R.color.hint))
                    setPadding(0, dp(28), 0, 0)
                },
            )
        }

        show(scrolling(layout))
    }

    /** What this build is, so an install can be told from the one before it. */
    private fun version(): String {
        val app = packageManager.getPackageInfo(packageName, 0).versionName ?: "?"
        return "app $app  ·  core ${uniffi.manymux_android.coreVersion()}"
    }

    // ---- what is running -----------------------------------------------

    /** The main screen: everything running on that machine. */
    private fun listSessions(machine: Machine) {
        show(overview(machine, column().apply { addView(note("reaching ${machine.address}")) }))

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
                    onFailure = { why -> trouble(machine, why) },
                )
                show(overview(machine, body))
            }
        }
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
            textSize = 11f
            setTextColor(colour(R.color.hint))
        }
        val words = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            addView(name)
            addView(who)
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

    private fun sessions(machine: Machine, running: List<Running>): View =
        LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            if (running.isEmpty()) {
                addView(note("Nothing is running there yet. The + starts a session."))
            }
            for (session in running) {
                addView(row(machine, session))
                addView(hairline())
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

    /** One session in the list: what it is called, what it is doing, how long ago. */
    private fun row(machine: Machine, session: Running): View {
        val what = if (session.title.isBlank()) session.command else session.title
        val words = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            addView(
                TextView(this@MainActivity).apply {
                    text = session.name
                    typeface = Typeface.MONOSPACE
                    textSize = 16f
                    setTextColor(colour(R.color.text))
                },
            )
            addView(
                TextView(this@MainActivity).apply {
                    text = what
                    textSize = 12f
                    isSingleLine = true
                    setTextColor(colour(R.color.dim))
                    setPadding(0, dp(3), 0, 0)
                },
            )
        }
        val since = TextView(this).apply {
            // A dot for somebody else already in there, the way the mark row
            // draws one: two people typing into one session is worth knowing
            // before you start rather than after.
            text = if (session.attached > 0u) "● ${ago(session.idle)}" else ago(session.idle)
            textSize = 12f
            gravity = Gravity.END
            setTextColor(
                if (session.attached > 0u) colour(R.color.accent) else colour(R.color.hint),
            )
        }
        return LinearLayout(this).apply {
            orientation = LinearLayout.HORIZONTAL
            gravity = Gravity.CENTER_VERTICAL
            isClickable = true
            setBackgroundResource(pressable())
            setPadding(dp(20), dp(16), dp(20), dp(16))
            addView(words, LinearLayout.LayoutParams(0, WRAP_CONTENT).apply { weight = 1f })
            addView(since)
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
        say("starting one")
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
            typeface = Typeface.MONOSPACE
            setPadding(dp(14), dp(10), dp(14), dp(10))
            setBackgroundColor(colour(R.color.panel))
            setTextColor(colour(R.color.text))
            textSize = 12f
        }

        val screen = TerminalView(this)
        terminal = screen
        screen.onCannotScroll = { say("$name is on a machine too old to scroll back") }

        val layout = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            // The bar above and the keys below are both this colour, so the
            // strips the system bars leave at the top and bottom continue them.
            setBackgroundColor(colour(R.color.panel))
            addView(bar, LinearLayout.LayoutParams(MATCH_PARENT, WRAP_CONTENT))
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
            orientation = LinearLayout.HORIZONTAL
            setBackgroundColor(colour(R.color.panel))
        }
        fun key(label: String, press: (TextView) -> Unit) {
            val button = TextView(this).apply {
                text = label
                textSize = 13f
                gravity = Gravity.CENTER
                setTextColor(colour(R.color.text))
                setPadding(0, dp(14), 0, dp(14))
                isClickable = true
                setBackgroundResource(pressable())
            }
            button.setOnClickListener { press(button) }
            keys.addView(
                button,
                LinearLayout.LayoutParams(0, WRAP_CONTENT).apply { weight = 1f },
            )
        }
        key("esc") { screen.send(byteArrayOf(0x1b)) }
        key("tab") { screen.send(byteArrayOf(0x09)) }
        // Held rather than pressed with something: the next character typed is
        // the chord, which is the only way a soft keyboard can spell one. It
        // says so by staying lit, since a modifier you cannot see the state of
        // is one you press twice.
        key("ctrl") { button ->
            screen.control = !screen.control
            button.setTextColor(
                if (screen.control) colour(R.color.accent) else colour(R.color.text),
            )
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
        key("⌨") { screen.openKeyboard() }
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
        view.setOnApplyWindowInsetsListener { at, insets ->
            val edges = edges(insets)
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
                android.util.Log.i(
                    "manymux",
                    "insets: keyboard=${insets.getInsets(WindowInsets.Type.ime()).bottom}" +
                        " bars=${insets.getInsets(WindowInsets.Type.systemBars()).bottom}" +
                        " padding=${edges.bottom} height=${at.height}",
                )
            }
            at.setPadding(edges.left, edges.top, edges.right, edges.bottom)
            insets
        }
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
        textSize = 30f
        typeface = Typeface.create("sans-serif-light", Typeface.NORMAL)
        setTextColor(colour(R.color.text))
        setPadding(0, dp(20), 0, dp(6))
    }

    /** A small uppercase heading, which is what separates the sections. */
    private fun label(what: String) = TextView(this).apply {
        text = what.uppercase()
        textSize = 11f
        letterSpacing = 0.14f
        setTextColor(colour(R.color.accent))
        setPadding(0, dp(28), 0, dp(10))
    }

    private fun note(what: String) = TextView(this).apply {
        text = what
        textSize = 13f
        setTextColor(colour(R.color.dim))
        setPadding(dp(20), dp(20), dp(20), dp(8))
    }

    private fun field(what: String, filled: String, example: String) = EditText(this).apply {
        hint = example
        contentDescription = what
        setText(filled)
        setSingleLine()
        textSize = 15f
        setTextColor(colour(R.color.text))
        setHintTextColor(colour(R.color.hint))
        background = panel(R.color.raised)
        setPadding(dp(14), dp(14), dp(14), dp(14))
        layoutParams = wide().apply { bottomMargin = dp(8) }
    }

    /** The one button on a screen that says what the screen is for. */
    private fun primary(what: String, press: () -> Unit) = TextView(this).apply {
        text = what
        textSize = 15f
        gravity = Gravity.CENTER
        setTextColor(colour(R.color.ground))
        background = panel(R.color.accent)
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
        background = GradientDrawable().apply {
            setColor(Color.TRANSPARENT)
            cornerRadius = dp(6).toFloat()
            setStroke(dp(1), colour(R.color.line))
        }
        setPadding(dp(16), dp(13), dp(16), dp(13))
        isClickable = true
        setOnClickListener { press() }
        layoutParams = wide().apply { topMargin = dp(8) }
    }

    private fun hairline() = View(this).apply {
        setBackgroundColor(colour(R.color.line))
        layoutParams = LinearLayout.LayoutParams(MATCH_PARENT, dp(1))
    }

    private fun say(what: String) {
        Toast.makeText(this, what, Toast.LENGTH_SHORT).show()
    }
}
