package com.vphone.android

import android.graphics.Bitmap
import java.io.File
import java.nio.ByteBuffer
import java.nio.ByteOrder

/**
 * The GPU device's frame dumps (`REIMS_VGPU_FRAME_DUMP`, on with a VM's GPU diagnostics): the
 * bytes of the guest surface behind the frame on screen, read by the CPU with no GPU command
 * between. Beside a screenshot they show whether the picture goes wrong before the surface or
 * after it.
 *
 * A file is "RVGDUMP1", the width and the height as little-endian 32-bit integers, then one
 * R, G, B, A byte quadruple per pixel, rows from the top.
 */
object FrameDumps {
    /** Where the dumps are, under the app's files. */
    const val DIR = "runs/frames"
    private const val HEADER = 16
    private const val MAX_SIDE = 8192
    private val MAGIC = "RVGDUMP1".toByteArray()

    /** A dump's width and height; null for anything that is not a whole dump. */
    fun size(file: ByteArray): Pair<Int, Int>? {
        if (file.size < HEADER || !file.copyOfRange(0, MAGIC.size).contentEquals(MAGIC)) return null
        val header = ByteBuffer.wrap(file, MAGIC.size, 8).order(ByteOrder.LITTLE_ENDIAN)
        val width = header.int
        val height = header.int
        if (width !in 1..MAX_SIDE || height !in 1..MAX_SIDE) return null
        if (file.size.toLong() != HEADER + width.toLong() * height * 4) return null
        return width to height
    }

    /** Writes each dump in [from] as a PNG in [into], oldest first: the PNGs written. */
    fun toPngs(from: File, into: File): List<File> {
        val dumps = from.listFiles { f -> f.name.endsWith(".rgba") }.orEmpty().sortedBy { it.lastModified() }
        return dumps.mapIndexedNotNull { index, dump ->
            val bytes = runCatching { dump.readBytes() }.getOrNull() ?: return@mapIndexedNotNull null
            val (width, height) = size(bytes) ?: return@mapIndexedNotNull null
            val bitmap = Bitmap.createBitmap(width, height, Bitmap.Config.ARGB_8888)
            // ARGB_8888 holds its pixels as R, G, B, A bytes, as the dump does.
            bitmap.copyPixelsFromBuffer(ByteBuffer.wrap(bytes, HEADER, bytes.size - HEADER))
            val png = File(into, "guest-surface-${index + 1}.png")
            png.outputStream().use { bitmap.compress(Bitmap.CompressFormat.PNG, 100, it) }
            bitmap.recycle()
            png
        }
    }
}
