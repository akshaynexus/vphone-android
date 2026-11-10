package com.vphone.android

import android.content.Context
import android.net.ConnectivityManager
import android.net.LinkProperties
import android.net.Network

/**
 * The DNS servers of the phone's active network, for the VM's network. QEMU's NAT forwards iOS's
 * lookups to a server it reads from a resolv.conf, and Android has none: the Rust library keeps
 * one in the app's files (`net.rs`), from the servers given here, and adds public ones after
 * them. The NAT looks at the file again when it changes, so a switch between Wi-Fi and mobile
 * data reaches a running VM.
 */
object VmNetwork {
    private var callback: ConnectivityManager.NetworkCallback? = null

    private fun write(link: LinkProperties?) {
        val servers = link?.dnsServers.orEmpty().mapNotNull { it.hostAddress }
        Native.setDnsServers(servers.joinToString(" "))
    }

    /** Writes the servers of the network in use now, and again whenever that network changes. */
    @Synchronized
    fun start(context: Context) {
        val cm = context.getSystemService(ConnectivityManager::class.java)
        runCatching { write(cm.getLinkProperties(cm.activeNetwork)) }
            .onFailure { Native.setDnsServers("") }
        if (callback != null) return
        val cb = object : ConnectivityManager.NetworkCallback() {
            override fun onLinkPropertiesChanged(network: Network, link: LinkProperties) = write(link)
            override fun onLost(network: Network) = write(null)
        }
        runCatching { cm.registerDefaultNetworkCallback(cb) }.onSuccess { callback = cb }
    }

    @Synchronized
    fun stop(context: Context) {
        val cb = callback ?: return
        callback = null
        runCatching { context.getSystemService(ConnectivityManager::class.java).unregisterNetworkCallback(cb) }
    }
}
