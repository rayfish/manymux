package dev.manymux.phone

import android.content.Context
import android.view.View
import android.view.ViewGroup
import android.widget.EditText
import android.widget.TextView
import androidx.test.core.app.ActivityScenario
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import android.graphics.Rect
import android.graphics.Bitmap
import java.io.File
import java.io.FileOutputStream
import org.junit.Test
import org.junit.runner.RunWith
import uniffi.manymux_android.Machine

@RunWith(AndroidJUnit4::class)
class HostStoreTest {
    @Test
    fun savedHostCanBeAddedAndOpenedWithOneTap() {
        val context = InstrumentationRegistry.getInstrumentation().targetContext
        val preferences = context.getSharedPreferences("host-ui-test", Context.MODE_PRIVATE)
        preferences.edit().clear().commit()
        val scenario = ActivityScenario.launch(MainActivity::class.java)
        try {
            scenario.onActivity { activity ->
                val store = MainActivity::class.java.getDeclaredField("hosts")
                store.isAccessible = true
                store.set(activity, HostStore(preferences))
                val show = MainActivity::class.java.getDeclaredMethod("showMachine")
                show.isAccessible = true
                show.invoke(activity)
                fun views(): List<View> = descendants(activity.findViewById(android.R.id.content))
                fun fill(name: String, value: String) {
                    views().filterIsInstance<EditText>().single { it.contentDescription == name }.setText(value)
                }
                views().single { it.contentDescription == "add host" }.performClick()
                fill("address", "gpu-box.example")
                fill("port", "2222")
                fill("user", "user")
                views().filterIsInstance<TextView>().single { it.text == "save host" }.performClick()
                val host = Machine("gpu-box.example", 2222u, "user")
                assertEquals(listOf(host), HostStore(preferences).all())
                val current = MainActivity::class.java.getDeclaredField("machine")
                current.isAccessible = true
                assertEquals(null, current.get(activity))
                views().single { it.contentDescription == "connect user@gpu-box.example:2222" }.performClick()
                assertEquals(host, current.get(activity))
            }
        } finally {
            scenario.close()
            preferences.edit().clear().commit()
        }
    }

    @Test
    fun savedHostsAreVisibleAndSetupHasSeparatePages() {
        val instrumentation = InstrumentationRegistry.getInstrumentation()
        val context = instrumentation.targetContext
        val preferences = context.getSharedPreferences("host-layout-test", Context.MODE_PRIVATE)
        preferences.edit().clear().commit()
        val fixtures = listOf("gpu-box.example", "build-box.example", "host.example")
            .map { Machine(it, 22u, "user") }
        for (host in fixtures.reversed()) HostStore(preferences).remember(host)
        val scenario = ActivityScenario.launch(MainActivity::class.java)
        try {
            scenario.onActivity { activity ->
                val store = MainActivity::class.java.getDeclaredField("hosts")
                store.isAccessible = true
                store.set(activity, HostStore(preferences))
                val show = MainActivity::class.java.getDeclaredMethod("showMachine")
                show.isAccessible = true
                show.invoke(activity)
            }
            instrumentation.waitForIdleSync()
            scenario.onActivity { activity ->
                val views = descendants(activity.findViewById(android.R.id.content))
                assertTrue(views.filterIsInstance<EditText>().isEmpty())
                for (host in fixtures) {
                    val card = views.single { it.contentDescription == "connect ${host.user}@${host.address}" }
                    val bounds = Rect()
                    assertTrue(card.getGlobalVisibleRect(bounds))
                    assertEquals(card.height, bounds.height())
                }
            }
            val screenshot = instrumentation.uiAutomation.takeScreenshot()
            FileOutputStream(File(context.cacheDir, "hosts-ui.png")).use {
                assertTrue(screenshot.compress(Bitmap.CompressFormat.PNG, 100, it))
            }
            screenshot.recycle()
            scenario.onActivity { activity ->
                val views = descendants(activity.findViewById(android.R.id.content))
                views.single { it.contentDescription == "add host" }.performClick()
                var page = descendants(activity.findViewById(android.R.id.content))
                assertTrue(page.filterIsInstance<EditText>().single { it.contentDescription == "address" }.isShown)
                page.filterIsInstance<TextView>().single { it.text == "Back" }.performClick()
                page = descendants(activity.findViewById(android.R.id.content))
                page.single { it.contentDescription == "show SSH public key" }.performClick()
                page = descendants(activity.findViewById(android.R.id.content))
                assertTrue(page.filterIsInstance<TextView>().single {
                    it.text == "Add this public key to the host's authorized_keys to allow access."
                }.isShown)
                val goBack = MainActivity::class.java.getDeclaredMethod("goBack")
                goBack.isAccessible = true
                goBack.invoke(activity)
                page = descendants(activity.findViewById(android.R.id.content))
                assertTrue(page.any { it.contentDescription == "add host" })
                assertTrue(!activity.isFinishing)
            }
        } finally {
            scenario.close()
            preferences.edit().clear().commit()
        }
    }

    private fun descendants(view: View): List<View> = listOf(view) +
        if (view is ViewGroup) (0 until view.childCount).flatMap { descendants(view.getChildAt(it)) }
        else emptyList()

    @Test
    fun hostsSurviveReopeningAndCanBeSelectedAndRemoved() {
        val context = InstrumentationRegistry.getInstrumentation().targetContext
        val preferences = context.getSharedPreferences("host-store-test", Context.MODE_PRIVATE)
        preferences.edit().clear().commit()
        try {
            val first = Machine("gpu-box.example", 22u, "user")
            val second = Machine("build-box.example", 2222u, "user")
            val otherUser = Machine(first.address, first.port, "other")
            preferences.edit().putString("address", first.address)
                .putString("user", first.user).putInt("port", 22).commit()
            val store = HostStore(preferences)
            assertEquals(listOf(first), store.all())
            store.remember(second)
            assertEquals(listOf(second, first), HostStore(preferences).all())
            store.remember(first)
            assertEquals(listOf(first, second), HostStore(preferences).all())
            store.remember(otherUser)
            assertEquals(listOf(otherUser, first, second), HostStore(preferences).all())
            store.remove(first)
            assertEquals(listOf(otherUser, second), HostStore(preferences).all())
            store.remove(otherUser)
            store.remove(second)
            assertEquals(emptyList<Machine>(), HostStore(preferences).all())
        } finally {
            preferences.edit().clear().commit()
        }
    }
}
