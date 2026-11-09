package com.vphone.android

import android.app.Service
import android.content.Intent
import android.os.Binder
import android.os.IBinder
import android.os.Parcel
import android.util.Log
import android.view.Surface
import kotlin.concurrent.thread

/**
 * A process per QEMU run: a creation boots the VM several times and QEMU runs once per process.
 * Bound from [CreateService], which holds the foreground service; this one needs none of its own,
 * so it can start whatever state the phone is in.
 *
 * It also takes the Surface the live view ([CreateVmScreen]) draws into: the view is in another
 * process, and a Surface crosses a Binder as a Parcelable.
 */
class CreateVmService : Service() {
    @Volatile private var started = false
    private val lock = Any()
    private var surface: Surface? = null
    private var client: IBinder? = null
    private var deathRecipient: IBinder.DeathRecipient? = null

    private val binder = object : Binder() {
        override fun onTransact(code: Int, data: Parcel, reply: Parcel?, flags: Int): Boolean {
            when (code) {
                TX_SET_SURFACE -> {
                    data.enforceInterface(DESCRIPTOR)
                    val token = data.readStrongBinder()
                    val incoming = if (data.readInt() != 0) Surface.CREATOR.createFromParcel(data) else null
                    setSurface(token, incoming)
                }
                TX_SURFACE_CHANGED -> {
                    data.enforceInterface(DESCRIPTOR)
                    // The same window again: the library lays it out for its new size.
                    synchronized(lock) { surface?.let { Native.setSurface(it) } }
                }
                TX_CLEAR -> {
                    data.enforceInterface(DESCRIPTOR)
                    setSurface(null, null)
                }
                else -> return super.onTransact(code, data, reply, flags)
            }
            reply?.writeNoException()
            return true
        }
    }

    /** Replaces the Surface; the old one is let go once the renderer has released it. */
    private fun setSurface(token: IBinder?, incoming: Surface?) {
        synchronized(lock) {
            val oldSurface = surface
            val oldClient = client
            val oldRecipient = deathRecipient
            if (oldClient != null && oldRecipient != null) {
                runCatching { oldClient.unlinkToDeath(oldRecipient, 0) }
            }
            surface = incoming
            client = token
            deathRecipient = null
            Native.setSurface(incoming)
            oldSurface?.release()
            if (token != null && incoming != null) {
                // A view that crashes never says goodbye; its Surface must not stay in use.
                val recipient = IBinder.DeathRecipient { clearIf(token) }
                if (runCatching { token.linkToDeath(recipient, 0) }.isSuccess) {
                    deathRecipient = recipient
                } else {
                    clearIf(token)
                }
            }
        }
    }

    private fun clearIf(token: IBinder) {
        synchronized(lock) {
            if (client == token) setSurface(null, null)
        }
    }

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

    companion object {
        private const val NO_SPEC = "no VM spec is waiting"
        const val DESCRIPTOR = "com.vphone.android.CreateVmService"
        const val TX_SET_SURFACE = IBinder.FIRST_CALL_TRANSACTION
        const val TX_SURFACE_CHANGED = IBinder.FIRST_CALL_TRANSACTION + 1
        const val TX_CLEAR = IBinder.FIRST_CALL_TRANSACTION + 2
    }
}
