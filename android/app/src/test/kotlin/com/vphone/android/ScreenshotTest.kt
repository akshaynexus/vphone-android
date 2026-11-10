package com.vphone.android

import android.app.Application
import androidx.activity.ComponentActivity
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.List
import androidx.compose.material.icons.automirrored.filled.Send
import androidx.compose.material.icons.filled.Home
import androidx.compose.material.icons.filled.Lock
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.test.junit4.createAndroidComposeRule
import androidx.compose.ui.test.onRoot
import androidx.compose.ui.unit.dp
import com.github.takahirom.roborazzi.ExperimentalRoborazziApi
import com.github.takahirom.roborazzi.captureRoboImage
import com.github.takahirom.roborazzi.captureScreenRoboImage
import com.vphone.android.ui.AppDialog
import com.vphone.android.ui.ControlItem
import com.vphone.android.ui.CreatePickActions
import com.vphone.android.ui.CreatePickScreen
import com.vphone.android.ui.CreatePickUi
import com.vphone.android.ui.CreateProgressActions
import com.vphone.android.ui.CreateProgressScreen
import com.vphone.android.ui.CreateProgressUi
import com.vphone.android.ui.CreateStep
import com.vphone.android.ui.DialogSpec
import com.vphone.android.ui.Fact
import com.vphone.android.ui.FailureUi
import com.vphone.android.ui.MIB
import com.vphone.android.ui.MainActions
import com.vphone.android.ui.MainScreen
import com.vphone.android.ui.MainUi
import com.vphone.android.ui.ProgressCard
import com.vphone.android.ui.ProgressUi
import com.vphone.android.ui.SettingsScreen
import com.vphone.android.ui.StorageButton
import com.vphone.android.ui.StorageRow
import com.vphone.android.ui.StorageScreen
import com.vphone.android.ui.StorageSection
import com.vphone.android.ui.StorageUi
import com.vphone.android.ui.UpdateCard
import com.vphone.android.ui.UpdateUi
import com.vphone.android.ui.VIcons
import com.vphone.android.ui.VmBottomBar
import com.vphone.android.ui.VmCardUi
import com.vphone.android.ui.VmControlsMenu
import com.vphone.android.ui.VmEndPanel
import com.vphone.android.ui.VmLogScreen
import com.vphone.android.ui.VmSettings
import com.vphone.android.ui.VmStatus
import com.vphone.android.ui.VphoneTheme
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config
import org.robolectric.annotation.GraphicsMode
import java.io.File

/** About a Galaxy S24 Ultra's screen in dp. */
private const val PHONE = "w412dp-h915dp-xxhdpi"

/** The same width, long enough to show a whole scrolling screen. */
private const val TALL = "w412dp-h2200dp-xxhdpi"
private const val TABLET = "w1280dp-h800dp-xhdpi"
private const val CARD = "w412dp-h360dp-xxhdpi"

/**
 * Pictures of the screens, drawn on the JVM with made-up state, in light and dark: no phone, no
 * native library. They are written as PNGs under `android/build/ui-screenshots/` by
 * `scripts/ui-screenshots.sh`. The fallback colours are used, as there is no wallpaper here.
 */
@OptIn(ExperimentalRoborazziApi::class)
@RunWith(RobolectricTestRunner::class)
@GraphicsMode(GraphicsMode.Mode.NATIVE)
// A plain Application: the app's own loads the native library.
@Config(sdk = [35], application = Application::class, qualifiers = PHONE)
class ScreenshotTest {
    @get:Rule
    val compose = createAndroidComposeRule<ComponentActivity>()

    private val out = File(System.getProperty("vphone.screenshots") ?: "build/ui-screenshots").apply { mkdirs() }

    /** One picture of `content` filling the screen. `windows` also takes dialogs, which are windows of their own. */
    private fun shot(name: String, dark: Boolean, windows: Boolean = false, content: @Composable () -> Unit) {
        compose.setContent {
            VphoneTheme(darkTheme = dark, dynamicColor = false) {
                Surface(Modifier.fillMaxSize(), color = MaterialTheme.colorScheme.surface) { content() }
            }
        }
        compose.waitForIdle()
        val file = File(out, "$name-${if (dark) "dark" else "light"}.png").path
        if (windows) captureScreenRoboImage(file) else compose.onRoot().captureRoboImage(file)
    }

    private val vms = listOf(
        VmCardUi("iPhone 16 · iOS 26.1", emptyList(), 1179, 2556, 6, 6144, 64L * 1024 * MIB, VmStatus.Running("Running", "12:04", true)),
        VmCardUi("vphone-iPhone17-3-261", status = VmStatus.Creating("step 5 of 9, Restore iOS, about 2 h 05 min left", 0.42f, false)),
        VmCardUi("Test bench", emptyList(), 1179, 2556, 4, 4096, 32L * 1024 * MIB),
        VmCardUi("Imported from the Pixel", listOf("Disk.img is missing", "nvram.bin is missing"), 1179, 2556, 6, 6144, 0),
    )

    private val transfer = ProgressUi(
        title = "Packing Test bench",
        detail = "Disk.img",
        fraction = 0.37f,
        stats = listOf("4212 of 11380 MB", "86.4 MB/s", "about 1 min 22 s left", "file so far 2950 MB"),
        cancelLabel = "Cancel",
    )

    private fun main(update: UpdateUi = UpdateUi.Hidden, failure: FailureUi? = null, sending: ProgressUi? = null) = MainUi(
        version = "0.6.5",
        device = "8 CPU cores, 11 GiB RAM, 6 GiB free",
        vms = vms,
        vmsPath = "/storage/emulated/0/Android/data/com.vphone.android/files/vms/<name>/",
        update = update,
        failure = failure,
        transfer = sending,
    )

    @Test fun vmListLight() = shot("vm-list", false) { MainScreen(main(), MainActions()) }
    @Test fun vmListDark() = shot("vm-list", true) { MainScreen(main(), MainActions()) }

    @Test fun vmListAddMenuLight() = shot("vm-list-add-menu", false) { MainScreen(main(), MainActions(), fabInitiallyOpen = true) }
    @Test fun vmListAddMenuDark() = shot("vm-list-add-menu", true) { MainScreen(main(), MainActions(), fabInitiallyOpen = true) }

    @Test fun vmListEmptyLight() = shot("vm-list-empty", false) { MainScreen(main().copy(vms = emptyList()), MainActions()) }
    @Test fun vmListEmptyDark() = shot("vm-list-empty", true) { MainScreen(main().copy(vms = emptyList()), MainActions()) }

    @Config(qualifiers = TABLET)
    @Test fun vmListTabletLight() = shot("vm-list-tablet", false) { MainScreen(main(UpdateUi.Available("0.6.6", 187)), MainActions()) }

    @Config(qualifiers = TABLET)
    @Test fun vmListTabletDark() = shot("vm-list-tablet", true) { MainScreen(main(UpdateUi.Available("0.6.6", 187)), MainActions()) }

    private val failure = FailureUi(
        "Test bench",
        "Android's per-app memory limit ended the VM. Lower its RAM or code cache. (MemoryLimiter:AnonSwap 9216 MB)",
        true,
    )

    @Test fun vmListFailureLight() = shot("vm-list-failure", false) { MainScreen(main(failure = failure).copy(vms = vms.drop(2)), MainActions()) }
    @Test fun vmListFailureDark() = shot("vm-list-failure", true) { MainScreen(main(failure = failure).copy(vms = vms.drop(2)), MainActions()) }

    private val settings = VmSettings(vcpus = 6, ramMib = 6144, tbMib = 2048)

    @Config(qualifiers = TALL)
    @Test fun settingsLight() = shot("vm-settings", false) { SettingsScreen("Test bench", settings, 8, 11264, {}, {}) }

    @Config(qualifiers = TALL)
    @Test fun settingsDark() = shot("vm-settings", true) { SettingsScreen("Test bench", settings, 8, 11264, {}, {}) }

    @Composable
    private fun Cards(content: @Composable () -> Unit) = Box(Modifier.padding(16.dp)) { content() }

    @Config(qualifiers = CARD)
    @Test fun transferLight() = shot("transfer-card", false) { Cards { ProgressCard(transfer, Icons.AutoMirrored.Filled.Send) } }

    @Config(qualifiers = CARD)
    @Test fun transferDark() = shot("transfer-card", true) { Cards { ProgressCard(transfer, Icons.AutoMirrored.Filled.Send) } }

    @Test fun vmListTransferLight() = shot("vm-list-transfer", false) { MainScreen(main(sending = transfer).copy(vms = vms.drop(2).map { if (it.name == "Test bench") it.copy(status = VmStatus.Sending(0.37f)) else it }), MainActions()) }
    @Test fun vmListTransferDark() = shot("vm-list-transfer", true) { MainScreen(main(sending = transfer).copy(vms = vms.drop(2).map { if (it.name == "Test bench") it.copy(status = VmStatus.Sending(0.37f)) else it }), MainActions()) }

    @Config(qualifiers = CARD)
    @Test fun updateAvailableLight() = shot("update-card-available", false) { Cards { UpdateCard(UpdateUi.Available("0.6.6", 187), MainActions()) } }

    @Config(qualifiers = CARD)
    @Test fun updateAvailableDark() = shot("update-card-available", true) { Cards { UpdateCard(UpdateUi.Available("0.6.6", 187), MainActions()) } }

    private val downloading = UpdateUi.Downloading("0.6.6", 81 * MIB, 187 * MIB, 9 * MIB + MIB / 2)

    @Config(qualifiers = CARD)
    @Test fun updateDownloadingLight() = shot("update-card-downloading", false) { Cards { UpdateCard(downloading, MainActions()) } }

    @Config(qualifiers = CARD)
    @Test fun updateDownloadingDark() = shot("update-card-downloading", true) { Cards { UpdateCard(downloading, MainActions()) } }

    private val updateFailed = UpdateUi.Failed(
        "the APK's SHA-256 is 9f2c41d0a77b3e6c58e0d1b2a4c3f5e6978812ab34cd56ef7890ab12cd34ef56, not the release's " +
            "1b7e0c9d3a5f42e8b6c1d0e9f8a7b6c5d4e3f2a1b0c9d8e7f6a5b4c3d2e1f0a9; it was deleted",
        true,
    )

    @Config(qualifiers = CARD)
    @Test fun updateFailedLight() = shot("update-card-failed", false) { Cards { UpdateCard(updateFailed, MainActions()) } }

    @Config(qualifiers = CARD)
    @Test fun updateFailedDark() = shot("update-card-failed", true) { Cards { UpdateCard(updateFailed, MainActions()) } }

    private val error = DialogSpec.error(
        "Test bench cannot start",
        "The VM asks for 6144 MiB of RAM and a 2048 MiB code cache, and this phone has 5120 MiB available now. " +
            "Close other apps, or lower the VM's RAM in its settings.\n\n" +
            "Disk.img is 34359738368 bytes, and 21474836480 were expected: the import did not finish " +
            "(/storage/emulated/0/Android/data/com.vphone.android/files/vms/Test bench/Disk.img).",
    )

    @Test fun errorDialogLight() = shot("error-dialog", false, windows = true) {
        MainScreen(main(), MainActions())
        AppDialog(error, onCopy = {}, onDismiss = {})
    }

    @Test fun errorDialogDark() = shot("error-dialog", true, windows = true) {
        MainScreen(main(), MainActions())
        AppDialog(error, onCopy = {}, onDismiss = {})
    }

    private val controls = listOf(
        ControlItem(Icons.Filled.Home, "Home") {},
        ControlItem(Icons.Filled.Lock, "Lock") {},
        ControlItem(VIcons.Log, "Logs") {},
        ControlItem(Icons.AutoMirrored.Filled.List, "VM list") {},
        ControlItem(VIcons.Power, "Shut down", danger = true) {},
        ControlItem(VIcons.Bar, "Use bar") {},
    )

    /** The VM screen is black around the guest's picture and always dark; a grey stands in for iOS here. */
    @Composable
    private fun Guest(content: @Composable () -> Unit) = Box(Modifier.fillMaxSize().background(Color.Black)) {
        Box(Modifier.fillMaxSize().padding(vertical = 24.dp).background(Color(0xFF3A4A63)))
        content()
    }

    @Test fun vmControlsMenu() = shot("vm-controls-menu", true) {
        Guest { VmControlsMenu("iPhone 16 · iOS 26.1: Running · 12:04", "Hold the button for Home. Drag it anywhere.", controls, {}, animate = false) }
    }

    @Config(qualifiers = "w412dp-h56dp-xxhdpi")
    @Test fun vmBottomBar() = shot("vm-bottom-bar", true) { VmBottomBar({}, {}, {}) }

    @Test fun vmEndPanel() = shot("vm-end-panel", true) {
        VmEndPanel("The VM could not start", "QEMU ended with status 1: could not open Disk.img: Permission denied", {}, {}, {})
    }

    private val storage = StorageUi(
        summary = "Phone: 96.2 GB free of 512 GB\nThis app uses 58.4 GB: VMs 41.0 GB, firmware 12.1 GB, leftovers 5.3 GB",
        used = 0.81f,
        sections = listOf(
            StorageSection(
                "Virtual machines",
                "No VMs on this phone.",
                listOf(
                    StorageRow(
                        "Test bench", "iPhone17,3, iOS 26.1\nReady to boot\n20.4 GB on disk, 310 MB of it logs", "",
                        listOf(StorageButton("View log", true) {}, StorageButton("Clear logs", true) {}, StorageButton("Reset", true) {}, StorageButton("Delete", true, true) {}),
                    ),
                    StorageRow(
                        "iPhone 16 · iOS 26.1", "iPhone17,3, iOS 26.1\nRunning\n20.6 GB on disk, 88 MB of it logs", "It is running. Shut it down first.",
                        listOf(StorageButton("Clear logs", false) {}, StorageButton("Reset", false) {}, StorageButton("Delete", false, true) {}),
                    ),
                ),
            ),
            StorageSection(
                "Firmware downloads", "The firmware cache is empty.",
                listOf(StorageRow("iPhone17,3_26.1_23B85_Restore.ipsw", "9.8 GB", "", listOf(StorageButton("Delete", true, true) {}))),
                StorageButton("Delete all firmware", true, true) {},
            ),
            StorageSection("Leftovers", "Nothing is left over.", emptyList()),
        ),
        resetText = "Removes every VM, the firmware cache and all leftovers (58.4 GB in total), and returns the app to its first-run state.",
        reset = StorageButton("Reset everything…", true, true) {},
    )

    @Config(qualifiers = TALL)
    @Test fun storageLight() = shot("storage", false) { StorageScreen(storage) {} }

    @Config(qualifiers = TALL)
    @Test fun storageDark() = shot("storage", true) { StorageScreen(storage) {} }

    private val pick = CreatePickUi(
        about = "Builds a VM from Apple's firmware with no computer and no root. The pairing marked tested has been verified end to end; others may need extra work.",
        devices = listOf("iPhone 16 (iPhone17,3)"),
        versions = listOf("iOS 26.1 (build 23B85), tested, recommended"),
        pairingNote = "Verified end to end with this app.",
        name = "vphone-iPhone17-3-261",
        trimNote = "On: during the first boot about 137 of iOS's background services are disabled, which a VM has no use for.",
        estimate = "Expect about 9 hours on this phone.",
        storage = "Storage free: 96 GB (about 60 GB will be used).",
        warnings = listOf("The phone is not on Wi-Fi: the download waits for it."),
    )

    @Config(qualifiers = TALL)
    @Test fun createPickLight() = shot("create-pick", false) { CreatePickScreen(pick, CreatePickActions()) }

    @Config(qualifiers = TALL)
    @Test fun createPickDark() = shot("create-pick", true) { CreatePickScreen(pick, CreatePickActions()) }

    private val creating = CreateProgressUi(
        name = "vphone-iPhone17-3-261",
        subtitle = "iPhone17,3, iOS 26.1",
        nowStep = "Step 2 of 9 · Download the firmware",
        eta = "Running. About 7 h 40 min left, going by this phone's last run of these steps.",
        message = "iPhone17,3_26.1_23B85_Restore.ipsw: 4.1 of 9.8 GB (11 MB/s)",
        fraction = 0.42f,
        busy = true,
        steps = listOf(
            CreateStep("tools", "Prepare the tools", "done", false, "2 min"),
            CreateStep("firmware", "Download the firmware", "running", true, "6 min so far; 14 min last time"),
            CreateStep("patch", "Patch the firmware", "pending", false),
            CreateStep("restore", "Restore iOS", "pending", false),
            CreateStep("firstboot", "First boot", "pending", false),
        ),
        facts = listOf(Fact("Stage", "downloading", wide = true), Fact("Pieces", "212 of 507"), Fact("Retries", "0")),
        machine = listOf(Fact("Free RAM", "5.1 GB"), Fact("Free storage", "96 GB"), Fact("Battery", "44.0 °C", warn = true), Fact("Thermal state", "light")),
        showCancel = true,
        pauseLabel = "Pause",
    )

    @Config(qualifiers = TALL)
    @Test fun createProgressLight() = shot("create-progress", false) { CreateProgressScreen(creating, CreateProgressActions()) }

    @Config(qualifiers = TALL)
    @Test fun createProgressDark() = shot("create-progress", true) { CreateProgressScreen(creating, CreateProgressActions()) }

    private val log = (1..60).joinToString("\n") { "[%5d.%03d] vphone: line $it of the VM's log, as QEMU and the status line write it".format(it * 3, it * 17 % 1000) }

    @Test fun vmLogLight() = shot("vm-log", false) { VmLogScreen("Test bench: Running for 12:04", log, true, {}, {}, {}, {}) }
    @Test fun vmLogDark() = shot("vm-log", true) { VmLogScreen("Test bench: Running for 12:04", log, true, {}, {}, {}, {}) }
}
