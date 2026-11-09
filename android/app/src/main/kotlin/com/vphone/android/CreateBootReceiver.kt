package com.vphone.android

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent

/** After a reboot (or an update) a creation that was running carries on. */
class CreateBootReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        val name = Native.createPending()
        if (name.isEmpty()) return
        // A creation the person cancelled or that failed waits for them to resume it.
        val state = org.json.JSONObject(Native.createStatus(name)).optString("state")
        if (state == "running" || state == "interrupted") CreateService.resumeAfterBoot(context, name)
    }
}
