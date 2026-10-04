package dev.manymux.phone

import android.content.SharedPreferences
import org.json.JSONArray
import org.json.JSONException
import org.json.JSONObject
import uniffi.manymux_android.Machine

/** Saved destinations, with the most recently selected host first. */
class HostStore(private val preferences: SharedPreferences) {
    fun all(): List<Machine> {
        val saved = preferences.getString("hosts", null)
        if (saved != null) {
            try {
                val array = JSONArray(saved)
                return (0 until array.length()).mapNotNull { index ->
                    val host = array.optJSONObject(index) ?: return@mapNotNull null
                    val address = host.optString("address")
                    val user = host.optString("user")
                    val port = host.optInt("port", 22)
                    if (address.isBlank() || user.isBlank() || port !in 1..65535) null
                    else Machine(address, port.toUShort(), user)
                }.distinct()
            } catch (_: JSONException) {
                // The previous single-host settings can still be recovered.
            }
        }
        val address = preferences.getString("address", "") ?: ""
        val user = preferences.getString("user", "") ?: ""
        val port = preferences.getInt("port", 22)
        return if (address.isBlank() || user.isBlank() || port !in 1..65535) emptyList()
        else listOf(Machine(address, port.toUShort(), user))
    }

    fun remember(machine: Machine) = write(listOf(machine) + all().filter { it != machine })

    fun remove(machine: Machine) = write(all().filter { it != machine })

    private fun write(hosts: List<Machine>) {
        val array = JSONArray()
        for (host in hosts) array.put(JSONObject().apply {
            put("address", host.address)
            put("port", host.port.toInt())
            put("user", host.user)
        })
        preferences.edit().putString("hosts", array.toString())
            .remove("address").remove("port").remove("user").apply()
    }
}
