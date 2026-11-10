package com.vphone.android

import android.app.PendingIntent
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.pm.PackageInstaller
import android.content.pm.PackageManager
import android.net.ConnectivityManager
import android.net.NetworkCapabilities
import org.json.JSONArray
import java.io.File
import java.io.FileOutputStream
import java.net.HttpURLConnection
import java.net.URL
import java.security.MessageDigest
import kotlin.concurrent.thread

/**
 * Updates from the project's GitHub releases: finds a newer `android-debug-X.Y.Z` release, downloads
 * its APK (resuming a partial download), checks it is this app signed with the same key and newer,
 * and installs it with PackageInstaller. A downloaded APK is deleted once the app runs as that
 * version or a later one.
 */
object Updater {
    private const val REPO = "akshaynexus/vphone-android"
    private const val TAG_PREFIX = "android-debug-"
    private const val CHECK_EVERY_MS = 6 * 60 * 60 * 1000L
    private const val PREFS = "update"

    class Release(
        val version: String,
        val tag: String,
        val notes: String,
        val apkUrl: String,
        val size: Long,
        /** Lower-case hex, from the asset's `digest`; empty when GitHub gave none. */
        val sha256: String,
        val page: String,
    )

    sealed class State {
        object Idle : State()
        object Checking : State()
        class UpToDate(val manual: Boolean) : State()
        class Available(val release: Release) : State()
        class Downloading(val release: Release, val done: Long, val total: Long, val bytesPerSec: Long) : State()
        class Ready(val release: Release, val apk: File) : State()
        class Installing(val release: Release) : State()
        class Failed(val message: String, val release: Release?) : State()
    }

    @Volatile var state: State = State.Idle
        private set
    @Volatile private var cancelled = false
    @Volatile private var busy = false

    /** An installer confirmation Android asked for while no screen of ours could show it. */
    @Volatile var pendingConfirm: Intent? = null

    private fun dir(context: Context) = File(context.filesDir, "updates").apply { mkdirs() }
    private fun apkFile(context: Context, version: String) = File(dir(context), "vphone-$version.apk")

    /** Deletes downloaded APKs this version has caught up with: run at every launch. */
    fun cleanup(context: Context) {
        val current = parse(BuildConfig.VERSION_NAME) ?: return
        dir(context).listFiles()?.forEach { f ->
            val version = f.name.removePrefix("vphone-").removeSuffix(".apk").removeSuffix(".part")
            val v = parse(version)
            if (v == null || compare(v, current) <= 0) f.delete()
        }
    }

    /** The launch's check: at most every few hours, quietly. */
    fun checkIfDue(context: Context) {
        val prefs = context.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
        if (System.currentTimeMillis() - prefs.getLong("checkedAt", 0) < CHECK_EVERY_MS) {
            // A download that the app's restart interrupted is picked up again.
            if (state is State.Idle) prefs.getString("found", null)?.let { check(context, manual = false) }
            return
        }
        check(context, manual = false)
    }

    fun check(context: Context, manual: Boolean) {
        if (busy) return
        busy = true
        if (manual || state is State.Idle || state is State.UpToDate || state is State.Failed) state = State.Checking
        val app = context.applicationContext
        thread(name = "update-check") {
            state = try {
                val release = newest()
                val prefs = app.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
                prefs.edit().putLong("checkedAt", System.currentTimeMillis()).putString("found", release?.version).apply()
                when {
                    release == null -> State.UpToDate(manual)
                    apkFile(app, release.version).isFile -> State.Ready(release, apkFile(app, release.version))
                    else -> State.Available(release)
                }
            } catch (e: Exception) {
                if (manual) State.Failed("Cannot check for updates: ${e.message ?: e}", null) else State.Idle
            }
            busy = false
            // On Wi-Fi or another unmetered network the APK comes down without asking.
            (state as? State.Available)?.let { if (unmetered(app)) download(app, it.release) }
        }
    }

    private fun unmetered(context: Context): Boolean {
        val cm = context.getSystemService(ConnectivityManager::class.java)
        val caps = cm.getNetworkCapabilities(cm.activeNetwork) ?: return false
        return caps.hasCapability(NetworkCapabilities.NET_CAPABILITY_NOT_METERED)
    }

    private fun connect(url: String, range: Long = 0): HttpURLConnection =
        (URL(url).openConnection() as HttpURLConnection).apply {
            connectTimeout = 15_000
            readTimeout = 30_000
            instanceFollowRedirects = true
            setRequestProperty("User-Agent", "vphone-android/${BuildConfig.VERSION_NAME}")
            setRequestProperty("Accept", if (url.contains("api.github.com")) "application/vnd.github+json" else "application/octet-stream")
            if (range > 0) setRequestProperty("Range", "bytes=$range-")
        }

    /** The newest release above this version that carries an APK, or null. */
    private fun newest(): Release? {
        val c = connect("https://api.github.com/repos/$REPO/releases?per_page=30")
        val body = try {
            if (c.responseCode != 200) throw IllegalStateException("GitHub answered ${c.responseCode} ${c.responseMessage}")
            c.inputStream.bufferedReader().readText()
        } finally {
            c.disconnect()
        }
        val current = parse(BuildConfig.VERSION_NAME) ?: return null
        return releases(body).filter { compare(parse(it.version)!!, current) > 0 }
            .maxWithOrNull { a, b -> compare(parse(a.version)!!, parse(b.version)!!) }
    }

    /** The releases in GitHub's list that are ours and have an APK. */
    fun releases(json: String): List<Release> {
        val all = JSONArray(json)
        val out = ArrayList<Release>()
        for (i in 0 until all.length()) {
            val r = all.getJSONObject(i)
            if (r.optBoolean("draft")) continue
            val tag = r.optString("tag_name")
            if (!tag.startsWith(TAG_PREFIX)) continue
            val version = tag.removePrefix(TAG_PREFIX)
            if (parse(version) == null) continue
            val assets = r.optJSONArray("assets") ?: continue
            val apk = (0 until assets.length()).map { assets.getJSONObject(it) }
                .firstOrNull { it.optString("name").endsWith(".apk") } ?: continue
            out += Release(
                version = version,
                tag = tag,
                notes = r.optString("body"),
                apkUrl = apk.getString("browser_download_url"),
                size = apk.optLong("size"),
                sha256 = apk.optString("digest").removePrefix("sha256:").lowercase().takeIf { it.length == 64 }.orEmpty(),
                page = r.optString("html_url"),
            )
        }
        return out
    }

    fun parse(version: String): List<Int>? {
        val parts = version.split('.')
        if (parts.isEmpty() || parts.size > 4) return null
        return parts.map { it.toIntOrNull() ?: return null }
    }

    fun compare(a: List<Int>, b: List<Int>): Int {
        for (i in 0 until maxOf(a.size, b.size)) {
            val d = a.getOrElse(i) { 0 } - b.getOrElse(i) { 0 }
            if (d != 0) return d
        }
        return 0
    }

    fun cancel() {
        cancelled = true
    }

    fun download(context: Context, release: Release) {
        if (busy) return
        busy = true
        cancelled = false
        val app = context.applicationContext
        thread(name = "update-download") {
            state = try {
                val apk = fetch(app, release)
                verify(app, apk)
                State.Ready(release, apk)
            } catch (e: Exception) {
                if (cancelled) State.Available(release) else State.Failed(e.message ?: e.toString(), release)
            }
            busy = false
        }
    }

    /** Downloads to a `.part` file, continuing one left by an earlier try. */
    private fun fetch(context: Context, release: Release): File {
        val target = apkFile(context, release.version)
        if (target.isFile) return target
        val part = File(target.path + ".part")
        var have = if (part.isFile) part.length() else 0L
        if (release.size in 1 until have) {
            part.delete()
            have = 0
        }
        val c = connect(release.apkUrl, have)
        try {
            val append = when (c.responseCode) {
                206 -> true
                200 -> false
                416 -> {
                    part.delete()
                    throw IllegalStateException("the partial download did not match; try again")
                }
                else -> throw IllegalStateException("the download answered ${c.responseCode} ${c.responseMessage}")
            }
            if (!append) have = 0
            val total = if (release.size > 0) release.size else have + c.contentLengthLong.coerceAtLeast(0)
            FileOutputStream(part, append).use { out ->
                c.inputStream.use { input ->
                    val buf = ByteArray(256 * 1024)
                    var done = have
                    var windowStart = System.nanoTime()
                    var windowBytes = 0L
                    var rate = 0L
                    while (true) {
                        if (cancelled) throw IllegalStateException("cancelled")
                        val n = input.read(buf)
                        if (n < 0) break
                        out.write(buf, 0, n)
                        done += n
                        windowBytes += n
                        val elapsed = System.nanoTime() - windowStart
                        if (elapsed >= 1_000_000_000L) {
                            rate = windowBytes * 1_000_000_000L / elapsed
                            windowStart = System.nanoTime()
                            windowBytes = 0
                        }
                        state = State.Downloading(release, done, total, rate)
                    }
                    out.fd.sync()
                }
            }
        } finally {
            c.disconnect()
        }
        if (release.size > 0 && part.length() != release.size) {
            throw IllegalStateException("the download stopped at ${part.length() / MIB} of ${release.size / MIB} MB; tap Download to continue")
        }
        if (release.sha256.isNotEmpty()) {
            val sum = sha256(part)
            if (sum != release.sha256) {
                part.delete()
                throw IllegalStateException("the APK's SHA-256 is $sum, not the release's ${release.sha256}; it was deleted")
            }
        }
        if (!part.renameTo(target)) throw IllegalStateException("cannot keep the APK at ${target.path}")
        return target
    }

    private fun sha256(file: File): String {
        val md = MessageDigest.getInstance("SHA-256")
        file.inputStream().use { input ->
            val buf = ByteArray(1 shl 20)
            while (true) {
                val n = input.read(buf)
                if (n < 0) break
                md.update(buf, 0, n)
            }
        }
        return md.digest().joinToString("") { "%02x".format(it) }
    }

    /** This app, newer, signed by the same key: what Android would refuse is said here first. */
    private fun verify(context: Context, apk: File) {
        val pm = context.packageManager
        val flags = PackageManager.PackageInfoFlags.of(PackageManager.GET_SIGNING_CERTIFICATES.toLong())
        val new = pm.getPackageArchiveInfo(apk.path, flags)
        if (new == null) {
            apk.delete()
            throw IllegalStateException("the download is not an APK Android can read; it was deleted")
        }
        if (new.packageName != context.packageName) {
            apk.delete()
            throw IllegalStateException("the APK is ${new.packageName}, not this app; it was deleted")
        }
        if (new.longVersionCode <= BuildConfig.VERSION_CODE) {
            throw IllegalStateException("the APK is version code ${new.longVersionCode}, not newer than this app's ${BuildConfig.VERSION_CODE}")
        }
        val installed = pm.getPackageInfo(context.packageName, flags)
        val mine = installed.signingInfo?.apkContentsSigners?.map { it.toCharsString() }?.toSet().orEmpty()
        val theirs = new.signingInfo?.apkContentsSigners?.map { it.toCharsString() }?.toSet().orEmpty()
        if (mine.isNotEmpty() && theirs.isNotEmpty() && mine != theirs) {
            throw IllegalStateException(
                "the APK is signed with a different key than this app, so Android will not install it over this one. " +
                    "Installing it means uninstalling vphone first, which deletes its VMs.",
            )
        }
    }

    /** Whether Android lets this app install packages; if not, the caller opens the setting. */
    fun canInstall(context: Context) = context.packageManager.canRequestPackageInstalls()

    fun install(context: Context, release: Release, apk: File) {
        if (busy) return
        busy = true
        state = State.Installing(release)
        val app = context.applicationContext
        thread(name = "update-install") {
            try {
                val installer = app.packageManager.packageInstaller
                val params = PackageInstaller.SessionParams(PackageInstaller.SessionParams.MODE_FULL_INSTALL).apply {
                    setAppPackageName(app.packageName)
                    setSize(apk.length())
                    // Once this app installed itself, later updates need no extra tap.
                    setRequireUserAction(PackageInstaller.SessionParams.USER_ACTION_NOT_REQUIRED)
                }
                val id = installer.createSession(params)
                installer.openSession(id).use { session ->
                    session.openWrite("base.apk", 0, apk.length()).use { out ->
                        apk.inputStream().use { it.copyTo(out, 1 shl 20) }
                        session.fsync(out)
                    }
                    val callback = PendingIntent.getBroadcast(
                        app, id, Intent(app, UpdateReceiver::class.java).putExtra(UpdateReceiver.EXTRA_VERSION, release.version),
                        PendingIntent.FLAG_MUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
                    )
                    session.commit(callback.intentSender)
                }
            } catch (e: Exception) {
                state = State.Failed("Cannot install: ${e.message ?: e}", release)
            }
            busy = false
        }
    }

    /** What PackageInstaller said about a commit. */
    internal fun installResult(context: Context, status: Int, message: String?, confirm: Intent?) {
        val release = (state as? State.Installing)?.release
        when (status) {
            PackageInstaller.STATUS_PENDING_USER_ACTION -> if (confirm != null) {
                confirm.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
                try {
                    context.startActivity(confirm)
                } catch (e: Exception) {
                    pendingConfirm = confirm
                }
            }
            // The app is replaced and restarted; nothing to do here.
            PackageInstaller.STATUS_SUCCESS -> Unit
            PackageInstaller.STATUS_FAILURE_ABORTED -> state = release?.let { r -> State.Ready(r, apkFile(context, r.version)) } ?: State.Idle
            else -> state = State.Failed("Android did not install the update: ${message ?: "status $status"}", release)
        }
    }

    private const val MIB = 1024L * 1024
}

class UpdateReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        val status = intent.getIntExtra(PackageInstaller.EXTRA_STATUS, PackageInstaller.STATUS_FAILURE)
        val message = intent.getStringExtra(PackageInstaller.EXTRA_STATUS_MESSAGE)
        val confirm = intent.getParcelableExtra(Intent.EXTRA_INTENT, Intent::class.java)
        Updater.installResult(context, status, message, confirm)
    }

    companion object {
        const val EXTRA_VERSION = "version"
    }
}
