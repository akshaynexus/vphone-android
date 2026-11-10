package com.vphone.android.ui

import androidx.activity.compose.BackHandler
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.WindowInsets
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.ime
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.layout.windowInsetsPadding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.ArrowBack
import androidx.compose.material3.Button
import androidx.compose.material3.ButtonGroupDefaults
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.ExperimentalMaterial3ExpressiveApi
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.ListItemDefaults
import androidx.compose.material3.ListItemShapes
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.MediumFlexibleTopAppBar
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.RadioButton
import androidx.compose.material3.Scaffold
import androidx.compose.material3.SegmentedListItem
import androidx.compose.material3.Slider
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.material3.ToggleButton
import androidx.compose.material3.TopAppBarDefaults
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.input.nestedscroll.nestedScroll
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.role
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.unit.dp
import kotlin.math.roundToInt

/** `VmConfig` of the native side (`vphone-native/src/config.rs`), field for field. */
data class VmSettings(
    val version: Int = 2,
    val vcpus: Int,
    val ramMib: Int,
    val tbMib: Int,
    val performance: String = "max",
    val refreshHz: Int = 120,
    val performanceHints: Boolean = true,
    val services: String = "unchanged",
    val network: Boolean = true,
    val sound: Boolean = true,
    val display: Boolean = true,
    val gpuCompression: Boolean = false,
    val gpuDiagnostics: Boolean = false,
    val softwareDisplay: Boolean = false,
    val swapchainLayout: String = "auto",
)

private typealias RowContent = @Composable (ListItemShapes) -> Unit

/**
 * One VM's settings, a screen of their own: grouped, each with what it does. Nothing is saved
 * until Save; leaving with changes asks first.
 */
@OptIn(ExperimentalMaterial3Api::class, ExperimentalMaterial3ExpressiveApi::class)
@Composable
fun SettingsScreen(
    name: String,
    initial: VmSettings,
    cpus: Int,
    totalMib: Long,
    onSave: (VmSettings) -> Unit,
    onBack: () -> Unit,
) {
    var s by rememberSaveable(stateSaver = SettingsSaver) { mutableStateOf(initial) }
    // The numbers as typed: an empty field is not yet a number.
    var ram by rememberSaveable { mutableStateOf(initial.ramMib.toString()) }
    var cache by rememberSaveable { mutableStateOf(initial.tbMib.toString()) }
    var leaving by rememberSaveable { mutableStateOf(false) }
    val edited = s.copy(ramMib = ram.toIntOrNull() ?: 0, tbMib = cache.toIntOrNull() ?: 0)
    val dirty = edited != initial
    BackHandler(dirty) { leaving = true }
    val scroll = TopAppBarDefaults.exitUntilCollapsedScrollBehavior()

    Scaffold(
        modifier = Modifier.nestedScroll(scroll.nestedScrollConnection),
        topBar = {
            MediumFlexibleTopAppBar(
                title = { Text(name) },
                subtitle = { Text(if (dirty) "Settings · not saved yet" else "Settings") },
                navigationIcon = {
                    IconButton(onClick = { if (dirty) leaving = true else onBack() }) {
                        Icon(Icons.AutoMirrored.Filled.ArrowBack, contentDescription = "Back")
                    }
                },
                actions = {
                    Button(onClick = { onSave(edited) }, enabled = dirty, modifier = Modifier.padding(end = 12.dp)) { Text("Save") }
                },
                scrollBehavior = scroll,
            )
        },
    ) { inner ->
        Box(Modifier.fillMaxSize().padding(inner).windowInsetsPadding(WindowInsets.ime), contentAlignment = Alignment.TopCenter) {
            Column(
                Modifier
                    .widthIn(max = 720.dp)
                    .verticalScroll(rememberScrollState())
                    .padding(start = 16.dp, end = 16.dp, bottom = 32.dp),
            ) {
                val ramValue = ram.toIntOrNull()
                val cacheValue = cache.toIntOrNull()
                Group(
                    "Performance",
                    {
                        val shown = s.vcpus.coerceIn(1, cpus.coerceAtLeast(1))
                        ControlRow(it, "vCPUs: $shown", "The emulated CPU cores iOS gets: 1 to $cpus on this phone.") {
                            Slider(
                                value = shown.toFloat(),
                                onValueChange = { v -> s = s.copy(vcpus = v.roundToInt()) },
                                valueRange = 1f..cpus.coerceAtLeast(2).toFloat(),
                                steps = (cpus - 2).coerceAtLeast(0),
                                enabled = cpus > 1,
                                modifier = Modifier.semantics { contentDescription = "vCPUs" },
                            )
                        }
                    },
                    {
                        ControlRow(
                            it,
                            "RAM",
                            "The memory iOS gets: at least 1024 MiB, and below the phone's $totalMib MiB. " +
                                "If Android ends the VM for memory, lower it.",
                        ) {
                            NumberField(ram, { v -> ram = v }, "RAM (MiB)", ramValue == null || ramValue < 1024 || ramValue >= totalMib)
                        }
                    },
                    {
                        ControlRow(it, "Performance", performanceNote(s.performance)) {
                            Choice(listOf("max" to "Max", "balanced" to "Balanced"), s.performance) { v -> s = s.copy(performance = v) }
                        }
                    },
                    {
                        SwitchRow(
                            it,
                            "Performance hints",
                            "Tell Android how busy the emulated CPUs are, through its Performance Hint service.",
                            s.performanceHints,
                        ) { v -> s = s.copy(performanceHints = v) }
                    },
                )
                Group(
                    "Display & GPU",
                    {
                        ControlRow(it, "Refresh rate", "iOS's display mode, and the rate asked of this phone's screen.") {
                            Choice(listOf("120" to "120 Hz", "60" to "60 Hz"), s.refreshHz.toString()) { v -> s = s.copy(refreshHz = v.toInt()) }
                        }
                    },
                    {
                        SwitchRow(it, "Show the screen", "Off: iOS runs with nothing drawn.", s.display) { v -> s = s.copy(display = v) }
                    },
                    {
                        SwitchRow(
                            it,
                            "GPU image compression",
                            "UBWC for the driver's own images. Faster where it works; turn it off if the picture is wrong.",
                            s.gpuCompression,
                        ) { v -> s = s.copy(gpuCompression = v) }
                    },
                )
                Group(
                    "iOS",
                    {
                        ControlRow(
                            it,
                            "Background services",
                            "Fewer leave more of the emulated CPUs to the screen. A change is applied at the VM's next " +
                                "start and takes effect at the start after that.",
                        ) {
                            RadioChoice(
                                listOf(
                                    Triple("unchanged", "As they are", ""),
                                    Triple("trimmed", "Trimmed", "About 138 off: Siri, iCloud sync, analytics, Journal…"),
                                    Triple("fewest", "Fewest", "Experimental: also no widgets, no AirDrop."),
                                ),
                                s.services,
                            ) { v -> s = s.copy(services = v) }
                        }
                    },
                )
                Group(
                    "Network & Sound",
                    {
                        SwitchRow(it, "Network", "Internet for iOS through this phone's connection; IPv4 only.", s.network) { v ->
                            s = s.copy(network = v)
                        }
                    },
                    {
                        SwitchRow(it, "Sound", "iOS plays through this phone's speaker or headphones; no microphone.", s.sound) { v ->
                            s = s.copy(sound = v)
                        }
                    },
                )
                Group(
                    "Advanced",
                    {
                        ControlRow(it, "Code cache", "Memory for the emulated CPUs' translated code: 64 to 2048 MiB.") {
                            NumberField(cache, { v -> cache = v }, "Code cache (MiB)", cacheValue == null || cacheValue !in 64..2048)
                        }
                    },
                    {
                        ControlRow(
                            it,
                            "Screen buffer layout",
                            "If the picture is smeared into bands or dashes, try the other two. Auto is what gralloc says, else linear.",
                        ) {
                            Choice(
                                listOf("auto" to "Auto", "linear" to "Linear", "ubwc" to "UBWC"),
                                s.swapchainLayout,
                            ) { v -> s = s.copy(swapchainLayout = v) }
                        }
                    },
                    {
                        SwitchRow(
                            it,
                            "Draw the screen on the CPU",
                            "Slower; right whatever the GPU driver does with the screen's buffers.",
                            s.softwareDisplay,
                        ) { v -> s = s.copy(softwareDisplay = v) }
                    },
                    {
                        SwitchRow(
                            it,
                            "GPU diagnostics",
                            "Image layouts in the log, and pictures of the guest's screen in the log screen's Share.",
                            s.gpuDiagnostics,
                        ) { v -> s = s.copy(gpuDiagnostics = v) }
                    },
                )
            }
        }
    }
    if (leaving) {
        AppDialog(
            DialogSpec(
                "Leave without saving?",
                "The changes to $name's settings have not been saved.",
                confirm = DialogAction("Save") { onSave(edited) },
                neutral = DialogAction("Discard", onBack),
                dismiss = DialogAction("Keep editing"),
            ),
            onCopy = {},
            onDismiss = { leaving = false },
        )
    }
}

private fun performanceNote(value: String) = if (value == "balanced") {
    "Balanced: the clocks follow the load, and Android holds them to what the phone can keep up. Cooler, slower."
} else {
    "Max: the highest CPU clocks whenever iOS is busy. Warmer, more battery."
}

@OptIn(ExperimentalMaterial3ExpressiveApi::class)
@Composable
private fun Group(title: String, vararg rows: RowContent) {
    SectionTitle(title)
    Column(verticalArrangement = Arrangement.spacedBy(ListItemDefaults.SegmentedGap)) {
        rows.forEachIndexed { i, row -> row(ListItemDefaults.segmentedShapes(i, rows.size)) }
    }
}

@OptIn(ExperimentalMaterial3ExpressiveApi::class)
@Composable
private fun SwitchRow(shapes: ListItemShapes, title: String, note: String, checked: Boolean, onChange: (Boolean) -> Unit) {
    SegmentedListItem(
        checked = checked,
        onCheckedChange = onChange,
        shapes = shapes,
        supportingContent = { Text(note) },
        // The whole row is the switch: the control itself is not a second target.
        trailingContent = { Switch(checked = checked, onCheckedChange = null) },
        modifier = Modifier.semantics { role = Role.Switch },
    ) { Text(title) }
}

@OptIn(ExperimentalMaterial3ExpressiveApi::class)
@Composable
private fun ControlRow(shapes: ListItemShapes, title: String, note: String, control: @Composable () -> Unit) {
    SegmentedListItem(
        shapes = shapes,
        supportingContent = {
            Column {
                Text(note)
                Box(Modifier.fillMaxWidth().padding(top = 8.dp)) { control() }
            }
        },
    ) { Text(title) }
}

@Composable
private fun NumberField(value: String, onChange: (String) -> Unit, label: String, wrong: Boolean) {
    OutlinedTextField(
        value = value,
        onValueChange = { v -> onChange(v.filter(Char::isDigit).take(6)) },
        label = { Text(label) },
        isError = wrong,
        singleLine = true,
        keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Number),
        modifier = Modifier.fillMaxWidth(),
    )
}

/** A few short choices, as connected toggle buttons. */
@OptIn(ExperimentalMaterial3ExpressiveApi::class)
@Composable
private fun Choice(options: List<Pair<String, String>>, selected: String, onSelect: (String) -> Unit) {
    Row(horizontalArrangement = Arrangement.spacedBy(ButtonGroupDefaults.ConnectedSpaceBetween)) {
        options.forEachIndexed { i, (value, label) ->
            ToggleButton(
                checked = value == selected,
                onCheckedChange = { onSelect(value) },
                modifier = Modifier.weight(1f).semantics { role = Role.RadioButton },
                shapes = when (i) {
                    0 -> ButtonGroupDefaults.connectedLeadingButtonShapes()
                    options.lastIndex -> ButtonGroupDefaults.connectedTrailingButtonShapes()
                    else -> ButtonGroupDefaults.connectedMiddleButtonShapes()
                },
            ) { Text(label, maxLines = 1) }
        }
    }
}

/** Choices that need a line of their own each. */
@OptIn(ExperimentalMaterial3ExpressiveApi::class)
@Composable
private fun RadioChoice(options: List<Triple<String, String, String>>, selected: String, onSelect: (String) -> Unit) {
    Column(verticalArrangement = Arrangement.spacedBy(ListItemDefaults.SegmentedGap)) {
        options.forEachIndexed { i, (value, label, note) ->
            SegmentedListItem(
                selected = value == selected,
                onClick = { onSelect(value) },
                shapes = ListItemDefaults.segmentedShapes(i, options.size),
                colors = ListItemDefaults.segmentedColors(containerColor = MaterialTheme.colorScheme.surfaceContainerHighest),
                leadingContent = { RadioButton(selected = value == selected, onClick = null) },
                supportingContent = if (note.isEmpty()) null else ({ Text(note) }),
                modifier = Modifier.semantics { role = Role.RadioButton },
            ) { Text(label) }
        }
    }
}

private val SettingsSaver = androidx.compose.runtime.saveable.listSaver<VmSettings, Any>(
    save = {
        listOf(
            it.version, it.vcpus, it.ramMib, it.tbMib, it.performance, it.refreshHz, it.performanceHints, it.services,
            it.network, it.sound, it.display, it.gpuCompression, it.gpuDiagnostics, it.softwareDisplay, it.swapchainLayout,
        )
    },
    restore = {
        VmSettings(
            it[0] as Int, it[1] as Int, it[2] as Int, it[3] as Int, it[4] as String, it[5] as Int, it[6] as Boolean,
            it[7] as String, it[8] as Boolean, it[9] as Boolean, it[10] as Boolean, it[11] as Boolean, it[12] as Boolean,
            it[13] as Boolean, it[14] as String,
        )
    },
)
