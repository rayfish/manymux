package dev.manymux.phone

import android.app.Application
import android.content.Intent
import android.os.Build
import uniffi.manymux_android.Attach
import uniffi.manymux_android.Grid
import uniffi.manymux_android.Machine
import uniffi.manymux_android.Phone

/** The client outlives its activity; SessionService keeps an open session running. */
class ManymuxApplication : Application() {
    val phone: Phone by lazy { Phone.keptIn(filesDir.absolutePath) }
    private val service by lazy { Intent(this, SessionService::class.java) }

    data class OpenSession(val machine: Machine, val name: String, val attach: Attach)

    var session: OpenSession? = null
        private set

    fun open(machine: Machine, name: String, grid: Grid): Attach {
        leave()
        val attached = phone.attach(machine, name, grid)
        session = OpenSession(machine, name, attached)
        try {
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) startForegroundService(service)
            else startService(service)
        } catch (why: RuntimeException) {
            leave()
            throw why
        }
        return attached
    }

    fun leave() {
        session?.attach?.let { it.detach(); it.close() }
        session = null
        // The service stops after promoting any pending foreground start.
    }
}
