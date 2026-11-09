package com.vphone.android

import android.app.Service
import android.content.Intent
import android.os.Binder
import android.os.IBinder
import android.util.Log
import kotlin.concurrent.thread

/**
 * A process per QEMU run: a creation boots the VM several times and QEMU runs once per process.
 * Bound from [CreateService], which holds the foreground service; this one needs none of its own,
 * so it can start whatever state the phone is in.
 */
class CreateVmService : Service() {
    private val binder = Binder()
    @Volatile private var started = false

    /**
     * A rebind to a service that already answered does not call onBind again, and Android
     * restarts a bound service after its process dies, so the spec is polled for rather than
     * tied to this call: the first spec written after this process came up starts it.
     */
    override fun onBind(intent: Intent?): IBinder {
        if (!started) {
            started = true
            thread(name = "qemu-start") {
                while (true) {
                    val error = Native.createVmRun()
                    if (error.isEmpty()) break
                    if (error != NO_SPEC) Log.w("vphone-create", "VM process: $error")
                    Thread.sleep(500)
                }
            }
        }
        return binder
    }

    private companion object {
        const val NO_SPEC = "no VM spec is waiting"
    }
}
