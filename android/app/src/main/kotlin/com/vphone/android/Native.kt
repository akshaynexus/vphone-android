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
}
