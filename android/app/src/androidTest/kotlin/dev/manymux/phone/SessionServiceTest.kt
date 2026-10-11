package dev.manymux.phone

import android.Manifest
import android.app.Notification
import android.app.NotificationManager
import android.os.Build
import android.os.SystemClock
import androidx.lifecycle.Lifecycle
import androidx.test.core.app.ActivityScenario
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import java.net.InetAddress
import java.net.ServerSocket
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertSame
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import uniffi.manymux_android.Machine

@RunWith(AndroidJUnit4::class)
class SessionServiceTest {
    @Test
    fun notificationAndSessionSurviveLeavingAndRecreatingTheActivity() {
        val instrumentation = InstrumentationRegistry.getInstrumentation()
        val context = instrumentation.targetContext
        val client = context.applicationContext as ManymuxApplication
        val notifications = context.getSystemService(NotificationManager::class.java)
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            instrumentation.uiAutomation.grantRuntimePermission(context.packageName, Manifest.permission.POST_NOTIFICATIONS)
        }
        // TCP connects, but no SSH banner arrives, keeping the first attach pending.
        ServerSocket(0, 1, InetAddress.getByName("127.0.0.1")).use { server ->
            val machine = Machine("127.0.0.1", server.localPort.toUShort(), "user")
            val scenario = ActivityScenario.launch(MainActivity::class.java)
            try {
                scenario.onActivity { activity ->
                    val open = MainActivity::class.java.getDeclaredMethod("open", Machine::class.java, String::class.java)
                    open.isAccessible = true
                    open.invoke(activity, machine, "build")
                }
                eventually { client.session != null }
                eventually { notifications.activeNotifications.isNotEmpty() }
                val attached = client.session!!.attach
                val notification = notifications.activeNotifications.single().notification
                assertTrue(notification.flags and Notification.FLAG_FOREGROUND_SERVICE != 0)
                assertTrue(notification.flags and Notification.FLAG_ONGOING_EVENT != 0)

                scenario.moveToState(Lifecycle.State.CREATED)
                assertSame(attached, client.session!!.attach)
                assertTrue(notifications.activeNotifications.isNotEmpty())
                notification.contentIntent.send()
                instrumentation.waitForIdleSync()
                scenario.moveToState(Lifecycle.State.RESUMED)
                scenario.recreate()
                instrumentation.waitForIdleSync()
                scenario.onActivity { activity ->
                    val field = MainActivity::class.java.getDeclaredField("attach")
                    field.isAccessible = true
                    assertSame(attached, field.get(activity))
                    assertEquals(machine, client.session!!.machine)
                    val leave = MainActivity::class.java.getDeclaredMethod("letGo")
                    leave.isAccessible = true
                    leave.invoke(activity)
                }
                eventually { notifications.activeNotifications.isEmpty() }
                assertNull(client.session)
            } finally {
                scenario.close()
                instrumentation.runOnMainSync { client.leave() }
            }
        }
    }

    private fun eventually(check: () -> Boolean) {
        val until = SystemClock.elapsedRealtime() + 5000
        while (!check() && SystemClock.elapsedRealtime() < until) SystemClock.sleep(25)
        assertTrue(check())
    }
}
