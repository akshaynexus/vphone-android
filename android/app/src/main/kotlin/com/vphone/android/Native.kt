package com.vphone.android

import android.view.Surface

/** The Rust library. Structured values cross as JSON; an error is its text, or "" for none. */
object Native {
    init {
        System.loadLibrary("vphone_native")
    }

    external fun setup(files: String, cache: String, external: String, nativeLib: String)
    external fun listVms(): String
    external fun deviceInfo(): String
    external fun saveVmConfig(name: String, configJson: String): String
    external fun preflight(name: String): String
    external fun boot(name: String): String
    external fun state(): String
    external fun takeNotice(): String
    external fun stop()
    external fun lastRun(): String
    external fun recordDeath(reason: String)
    external fun logTail(path: String, bytes: Long): String
    external fun setSurface(surface: Surface?)
    external fun layoutImage(width: Int, height: Int): IntArray
    external fun touch(action: Int, x: Float, y: Float)
    external fun home()
    external fun lock()
    external fun importBegin(folder: String, totalBytes: Long): String
    external fun importFile(fd: Int, relativePath: String): String
    external fun importFinish(): String
    external fun importAbort()
    external fun importCancel()
    external fun importProgress(): String

    // VM creation: see src/create/ in the Rust library.
    external fun createCatalog(): String
    external fun createPreflight(specJson: String): String
    external fun createBegin(specJson: String): String
    external fun createPending(): String
    external fun createStatus(name: String): String
    external fun createRun(name: String): String
    external fun createCancel(name: String)
    external fun createPause(name: String, paused: Boolean)
    external fun createAllowCellular(name: String, allow: Boolean)
    external fun createNetwork(connected: Boolean, unmetered: Boolean)
    external fun createThermal(status: Int)
    external fun createDelete(name: String, alsoFirmware: Boolean): String

    // The log screen: a view is a handle; rows, summaries and exports are JSON or text.
    external fun createLive(name: String): String
    external fun createLogOpen(path: String): Long
    external fun createLogClose(view: Long)
    external fun createLogSync(view: Long, runState: String, paused: Boolean): String
    external fun createLogRows(view: Long, first: Int, count: Int): String
    external fun createLogFilter(view: Long, filter: Int, search: String)
    external fun createLogToggle(view: Long, stage: Int)
    external fun createLogOpenAll(view: Long, open: Boolean)
    external fun createLogCopy(view: Long, stage: Int): String
    external fun createLogExport(view: Long, dest: String): String
}
