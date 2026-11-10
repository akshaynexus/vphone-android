package com.vphone.android

import android.content.Context
import android.hardware.usb.UsbConstants
import android.hardware.usb.UsbDevice
import android.hardware.usb.UsbManager
import android.mtp.MtpConstants
import android.mtp.MtpDevice

/**
 * `.vphone` files on another phone plugged in by USB: that phone is in "File transfer" (MTP)
 * mode, and this one controls the link (USB host). Android has no way for an app to be an MTP
 * device, but it can read from one with [MtpDevice]; the sending phone needs nothing but its
 * own File transfer mode, with the file saved somewhere it shares, such as Downloads.
 */
object UsbVmSource {
    const val ACTION_PERMISSION = "com.vphone.android.USB_PERMISSION"

    class Found(val device: String, val handle: Int, val name: String, val size: Long, val folder: String)

    /** Plugged-in devices that offer MTP: the still-image class, or an interface named MTP. */
    fun devices(context: Context): List<UsbDevice> =
        context.getSystemService(UsbManager::class.java).deviceList.values.filter { d ->
            (0 until d.interfaceCount).map { d.getInterface(it) }.any { i ->
                (i.interfaceClass == UsbConstants.USB_CLASS_STILL_IMAGE && i.interfaceSubclass == 1 && i.interfaceProtocol == 1) ||
                    i.name.equals("MTP", ignoreCase = true)
            }
        }

    fun label(d: UsbDevice) = listOfNotNull(d.manufacturerName, d.productName).joinToString(" ").ifEmpty { d.deviceName }

    /** Opens `device`, which this app must have USB permission for; the caller closes it. */
    fun open(context: Context, device: UsbDevice): MtpDevice {
        val connection = context.getSystemService(UsbManager::class.java).openDevice(device)
            ?: throw IllegalStateException("Android did not open the USB connection to ${label(device)}")
        val mtp = MtpDevice(device)
        if (!mtp.open(connection)) {
            connection.close()
            throw IllegalStateException(
                "${label(device)} does not answer as a file-transfer phone. On it, tap the USB notification and choose File transfer.",
            )
        }
        return mtp
    }

    /**
     * The `.vphone` files in the folders people save to: each storage's top level, then the
     * folders under it (skipping Android/, which holds apps' private data) to [MAX_DEPTH] levels,
     * at most [MAX_OBJECTS] objects in all.
     */
    fun find(mtp: MtpDevice, deviceName: String): List<Found> {
        val out = ArrayList<Found>()
        var visited = 0
        for (storage in mtp.storageIds ?: IntArray(0)) {
            val queue = ArrayDeque<Triple<Int, String, Int>>() // parent, path, depth
            queue.add(Triple(ROOT, "", 0))
            while (queue.isNotEmpty() && visited < MAX_OBJECTS) {
                val (parent, path, depth) = queue.removeFirst()
                val handles = mtp.getObjectHandles(storage, 0, parent) ?: continue
                for (h in handles) {
                    if (++visited > MAX_OBJECTS) break
                    val info = mtp.getObjectInfo(h) ?: continue
                    val name = info.name ?: continue
                    if (info.format == MtpConstants.FORMAT_ASSOCIATION) {
                        if (depth < MAX_DEPTH && !(depth == 0 && name.equals("Android", ignoreCase = true)) && !name.startsWith(".")) {
                            queue.add(Triple(h, "$path$name/", depth + 1))
                        }
                    } else if (name.endsWith(".vphone", ignoreCase = true)) {
                        out += Found(deviceName, h, name, info.compressedSizeLong, path.ifEmpty { "/" })
                    }
                }
            }
        }
        return out.sortedBy { it.name.lowercase() }
    }

    /** PTP's "objects at the storage's top level". */
    private const val ROOT = -1
    private const val MAX_DEPTH = 3
    private const val MAX_OBJECTS = 20_000
}
