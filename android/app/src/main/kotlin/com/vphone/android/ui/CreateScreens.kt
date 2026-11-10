package com.vphone.android.ui

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.ExperimentalLayoutApi
import androidx.compose.foundation.layout.FlowRow
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.WindowInsets
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.ime
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.layout.windowInsetsPadding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.text.selection.SelectionContainer
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.ArrowBack
import androidx.compose.material.icons.filled.Check
import androidx.compose.material.icons.filled.Close
import androidx.compose.material.icons.filled.PlayArrow
import androidx.compose.material.icons.filled.Warning
import androidx.compose.material3.Button
import androidx.compose.material3.ButtonDefaults
import androidx.compose.material3.ButtonGroupDefaults
import androidx.compose.material3.Card
import androidx.compose.material3.CardDefaults
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.ExperimentalMaterial3ExpressiveApi
import androidx.compose.material3.ExposedDropdownMenu
import androidx.compose.material3.ExposedDropdownMenuAnchorType
import androidx.compose.material3.ExposedDropdownMenuBox
import androidx.compose.material3.ExposedDropdownMenuDefaults
import androidx.compose.material3.FilledTonalButton
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.LinearWavyProgressIndicator
import androidx.compose.material3.ListItemDefaults
import androidx.compose.material3.LoadingIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.MediumFlexibleTopAppBar
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Scaffold
import androidx.compose.material3.SegmentedListItem
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.ToggleButton
import androidx.compose.material3.TopAppBarDefaults
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.alpha
import androidx.compose.ui.input.nestedscroll.nestedScroll
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.role
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.SpanStyle
import androidx.compose.ui.text.buildAnnotatedString
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.withStyle
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp

// ---------------------------------------------------------------------- the pickers

data class CreatePickUi(
    val about: String = "",
    val devices: List<String> = emptyList(),
    val device: Int = 0,
    val versions: List<String> = emptyList(),
    val version: Int = 0,
    val pairingNote: String = "",
    /** Said in red when the pairing has not been verified with this flow. */
    val warning: String = "",
    val name: String = "",
    val scale: Int = 1,
    val minScale: Int = 1,
    val cellular: Boolean = false,
    val deleteFirmware: Boolean = false,
    val trimServices: Boolean = true,
    val trimNote: String = "",
    val estimate: String = "",
    val storage: String = "",
    val errors: List<String> = emptyList(),
    val warnings: List<String> = emptyList(),
)

class CreatePickActions(
    val device: (Int) -> Unit = {},
    val version: (Int) -> Unit = {},
    val name: (String) -> Unit = {},
    val scale: (Int) -> Unit = {},
    val cellular: (Boolean) -> Unit = {},
    val deleteFirmware: (Boolean) -> Unit = {},
    val trimServices: (Boolean) -> Unit = {},
    val start: () -> Unit = {},
    val importInstead: () -> Unit = {},
    val back: () -> Unit = {},
)

@OptIn(ExperimentalMaterial3Api::class, ExperimentalMaterial3ExpressiveApi::class)
@Composable
fun CreatePickScreen(ui: CreatePickUi, actions: CreatePickActions) {
    val scroll = TopAppBarDefaults.exitUntilCollapsedScrollBehavior()
    Scaffold(
        modifier = Modifier.nestedScroll(scroll.nestedScrollConnection),
        topBar = {
            MediumFlexibleTopAppBar(
                title = { Text("Create a VM") },
                subtitle = { Text("From Apple's firmware, on this phone") },
                navigationIcon = {
                    IconButton(onClick = actions.back) { Icon(Icons.AutoMirrored.Filled.ArrowBack, contentDescription = "Back") }
                },
                scrollBehavior = scroll,
            )
        },
    ) { inner ->
        Box(Modifier.fillMaxSize().padding(inner).windowInsetsPadding(WindowInsets.ime), contentAlignment = Alignment.TopCenter) {
            Column(
                Modifier.widthIn(max = 720.dp).verticalScroll(rememberScrollState()).padding(start = 16.dp, end = 16.dp, bottom = 32.dp),
                verticalArrangement = Arrangement.spacedBy(12.dp),
            ) {
                Card(
                    shape = MaterialTheme.shapes.extraLarge,
                    colors = CardDefaults.cardColors(containerColor = MaterialTheme.colorScheme.secondaryContainer),
                ) { Text(ui.about, style = MaterialTheme.typography.bodyMedium, modifier = Modifier.padding(20.dp)) }

                Dropdown("Device", ui.devices, ui.device, actions.device)
                Dropdown("iOS version", ui.versions, ui.version, actions.version)
                if (ui.pairingNote.isNotEmpty()) {
                    Text(ui.pairingNote, style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
                }
                if (ui.warning.isNotEmpty()) {
                    Text(ui.warning, style = MaterialTheme.typography.bodyMedium, color = MaterialTheme.colorScheme.error)
                }
                OutlinedTextField(
                    value = ui.name,
                    onValueChange = actions.name,
                    label = { Text("VM name") },
                    singleLine = true,
                    modifier = Modifier.fillMaxWidth(),
                )
                Text("Screen scale", style = MaterialTheme.typography.labelLarge)
                Row(horizontalArrangement = Arrangement.spacedBy(ButtonGroupDefaults.ConnectedSpaceBetween)) {
                    (1..3).forEach { scale ->
                        ToggleButton(
                            checked = ui.scale == scale,
                            onCheckedChange = { actions.scale(scale) },
                            enabled = scale > 1 || ui.minScale <= 1,
                            modifier = Modifier.weight(1f).semantics { role = Role.RadioButton },
                            shapes = when (scale) {
                                1 -> ButtonGroupDefaults.connectedLeadingButtonShapes()
                                3 -> ButtonGroupDefaults.connectedTrailingButtonShapes()
                                else -> ButtonGroupDefaults.connectedMiddleButtonShapes()
                            },
                        ) { Text("${scale}x") }
                    }
                }
                Column(verticalArrangement = Arrangement.spacedBy(ListItemDefaults.SegmentedGap)) {
                    Toggle(0, "Allow downloads over cellular", "About 12 GB.", ui.cellular, actions.cellular)
                    Toggle(
                        1,
                        "Delete the firmware files when finished",
                        "12 GB back; another VM downloads them again.",
                        ui.deleteFirmware,
                        actions.deleteFirmware,
                    )
                    Toggle(2, "Turn off background services a VM has no use for", ui.trimNote, ui.trimServices, actions.trimServices)
                }
                Card(
                    shape = MaterialTheme.shapes.large,
                    colors = CardDefaults.cardColors(containerColor = MaterialTheme.colorScheme.surfaceContainerHigh),
                    modifier = Modifier.fillMaxWidth(),
                ) {
                    SelectionContainer {
                        Column(Modifier.padding(16.dp), verticalArrangement = Arrangement.spacedBy(6.dp)) {
                            if (ui.estimate.isNotEmpty()) Text(ui.estimate, style = MaterialTheme.typography.bodyMedium)
                            Text(ui.storage, style = MaterialTheme.typography.bodyMedium)
                            ui.errors.forEach { NoticeLine(it, error = true) }
                            ui.warnings.forEach { NoticeLine(it, error = false) }
                        }
                    }
                }
                Button(
                    onClick = actions.start,
                    enabled = ui.errors.isEmpty(),
                    modifier = Modifier.fillMaxWidth().heightIn(min = 56.dp),
                ) { Text("Create this VM") }
                TextButton(onClick = actions.importInstead, modifier = Modifier.fillMaxWidth()) {
                    Text("I already have a VM folder: import it instead")
                }
            }
        }
    }
}

@Composable
private fun NoticeLine(text: String, error: Boolean) {
    Row {
        Icon(
            if (error) VIcons.Error else Icons.Filled.Warning,
            contentDescription = if (error) "Error" else "Warning",
            tint = if (error) MaterialTheme.colorScheme.error else MaterialTheme.colorScheme.tertiary,
            modifier = Modifier.size(18.dp),
        )
        Spacer(Modifier.width(8.dp))
        Text(
            text,
            style = MaterialTheme.typography.bodyMedium,
            color = if (error) MaterialTheme.colorScheme.error else MaterialTheme.colorScheme.onSurface,
        )
    }
}

@OptIn(ExperimentalMaterial3ExpressiveApi::class)
@Composable
private fun Toggle(index: Int, title: String, note: String, checked: Boolean, onChange: (Boolean) -> Unit) {
    SegmentedListItem(
        checked = checked,
        onCheckedChange = onChange,
        shapes = ListItemDefaults.segmentedShapes(index, 3),
        supportingContent = if (note.isEmpty()) null else ({ Text(note) }),
        trailingContent = { Switch(checked = checked, onCheckedChange = null) },
        modifier = Modifier.semantics { role = Role.Switch },
    ) { Text(title) }
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable
private fun Dropdown(label: String, options: List<String>, selected: Int, onSelect: (Int) -> Unit) {
    var open by remember { mutableStateOf(false) }
    ExposedDropdownMenuBox(expanded = open, onExpandedChange = { open = it }) {
        OutlinedTextField(
            value = options.getOrElse(selected) { "" },
            onValueChange = {},
            readOnly = true,
            label = { Text(label) },
            trailingIcon = { ExposedDropdownMenuDefaults.TrailingIcon(expanded = open) },
            modifier = Modifier.fillMaxWidth().menuAnchor(ExposedDropdownMenuAnchorType.PrimaryNotEditable),
        )
        ExposedDropdownMenu(expanded = open, onDismissRequest = { open = false }) {
            options.forEachIndexed { i, option ->
                DropdownMenuItem(
                    text = { Text(option) },
                    onClick = {
                        open = false
                        onSelect(i)
                    },
                )
            }
        }
    }
}

// ---------------------------------------------------------------------- following one

data class CreateStep(val key: String, val title: String, val state: String, val current: Boolean, val time: String = "")

data class Fact(val label: String, val value: String, val warn: Boolean = false, val wide: Boolean = false)

data class CreateProgressUi(
    val name: String = "",
    val subtitle: String = "",
    val nowStep: String = "",
    val eta: String = "",
    val hold: String = "",
    val showCellular: Boolean = false,
    val thermal: String = "",
    val message: String = "",
    /** The failure, whole; null while there is none. */
    val error: String? = null,
    val barVisible: Boolean = true,
    /** Null: the step does not say how far it is. */
    val fraction: Float? = null,
    val busy: Boolean = false,
    val steps: List<CreateStep> = emptyList(),
    val phases: List<String> = emptyList(),
    val facts: List<Fact> = emptyList(),
    val machine: List<Fact> = emptyList(),
    val showLaunch: Boolean = false,
    val retryLabel: String? = null,
    val showCancel: Boolean = false,
    val pauseLabel: String? = null,
)

class CreateProgressActions(
    val copyError: () -> Unit = {},
    val liveLog: () -> Unit = {},
    val watchVm: () -> Unit = {},
    val pause: () -> Unit = {},
    val cancel: () -> Unit = {},
    val delete: () -> Unit = {},
    val retry: () -> Unit = {},
    val useCellular: () -> Unit = {},
    val launch: () -> Unit = {},
    val stepLog: (String) -> Unit = {},
    val back: () -> Unit = {},
)

@OptIn(ExperimentalMaterial3Api::class, ExperimentalMaterial3ExpressiveApi::class, ExperimentalLayoutApi::class)
@Composable
fun CreateProgressScreen(ui: CreateProgressUi, actions: CreateProgressActions) {
    val scroll = TopAppBarDefaults.exitUntilCollapsedScrollBehavior()
    Scaffold(
        modifier = Modifier.nestedScroll(scroll.nestedScrollConnection),
        topBar = {
            MediumFlexibleTopAppBar(
                title = { Text(ui.name.ifEmpty { "Creating a VM" }) },
                subtitle = if (ui.subtitle.isEmpty()) null else ({ Text(ui.subtitle) }),
                navigationIcon = {
                    IconButton(onClick = actions.back) { Icon(Icons.AutoMirrored.Filled.ArrowBack, contentDescription = "Back") }
                },
                scrollBehavior = scroll,
            )
        },
    ) { inner ->
        Box(Modifier.fillMaxSize().padding(inner), contentAlignment = Alignment.TopCenter) {
            Column(
                Modifier.widthIn(max = 720.dp).verticalScroll(rememberScrollState()).padding(start = 16.dp, end = 16.dp, bottom = 32.dp),
                verticalArrangement = Arrangement.spacedBy(12.dp),
            ) {
                Card(
                    shape = MaterialTheme.shapes.extraLarge,
                    colors = CardDefaults.cardColors(containerColor = MaterialTheme.colorScheme.primaryContainer),
                    modifier = Modifier.fillMaxWidth(),
                ) {
                    Column(Modifier.padding(20.dp)) {
                        Row(verticalAlignment = Alignment.CenterVertically) {
                            if (ui.busy) {
                                LoadingIndicator(Modifier.size(40.dp).semantics { contentDescription = "Working" })
                                Spacer(Modifier.width(12.dp))
                            }
                            Text(ui.nowStep, style = MaterialTheme.typography.titleLarge)
                        }
                        if (ui.message.isNotEmpty()) {
                            Text(ui.message, style = MaterialTheme.typography.bodyLarge, modifier = Modifier.padding(top = 8.dp))
                        }
                        if (ui.barVisible) {
                            val bar = Modifier.fillMaxWidth().padding(top = 14.dp).semantics { contentDescription = "This step's progress" }
                            val fraction = ui.fraction
                            val track = MaterialTheme.colorScheme.surface.copy(alpha = 0.7f)
                            if (fraction != null) {
                                LinearWavyProgressIndicator(progress = { fraction.coerceIn(0f, 1f) }, modifier = bar, trackColor = track)
                            } else if (ui.busy) {
                                LinearWavyProgressIndicator(modifier = bar, trackColor = track)
                            } else {
                                LinearWavyProgressIndicator(progress = { 0f }, modifier = bar, trackColor = track)
                            }
                        }
                        Text(ui.eta, style = MaterialTheme.typography.bodyMedium, modifier = Modifier.padding(top = 12.dp))
                        if (ui.hold.isNotEmpty()) {
                            Text(
                                ui.hold,
                                style = MaterialTheme.typography.bodyMedium,
                                fontWeight = FontWeight.Bold,
                                modifier = Modifier.padding(top = 6.dp),
                            )
                        }
                        if (ui.thermal.isNotEmpty()) {
                            Text(
                                ui.thermal,
                                style = MaterialTheme.typography.bodyMedium,
                                color = MaterialTheme.colorScheme.error,
                                modifier = Modifier.padding(top = 6.dp),
                            )
                        }
                    }
                }
                ui.error?.let { error ->
                    ErrorCard("Creation stopped with an error", error, onCopy = actions.copyError, copyLabel = "Copy error and log")
                }
                FlowRow(horizontalArrangement = Arrangement.spacedBy(8.dp), verticalArrangement = Arrangement.spacedBy(4.dp)) {
                    if (ui.showLaunch) {
                        Button(onClick = actions.launch) {
                            Icon(Icons.Filled.PlayArrow, contentDescription = null, modifier = Modifier.size(18.dp))
                            Spacer(Modifier.width(6.dp))
                            Text("Launch the VM")
                        }
                    }
                    ui.retryLabel?.let { Button(onClick = actions.retry) { Text(it) } }
                    if (ui.showCellular) FilledTonalButton(onClick = actions.useCellular) { Text("Download over cellular anyway") }
                    ui.pauseLabel?.let { FilledTonalButton(onClick = actions.pause) { Text(it) } }
                    FilledTonalButton(onClick = actions.liveLog) { Text("Live log") }
                    FilledTonalButton(onClick = actions.watchVm) { Text("VM activity") }
                    if (ui.showCancel) OutlinedButton(onClick = actions.cancel) { Text("Stop") }
                    OutlinedButton(
                        onClick = actions.delete,
                        colors = ButtonDefaults.outlinedButtonColors(contentColor = MaterialTheme.colorScheme.error),
                    ) { Text("Delete VM") }
                }

                SectionTitle("Steps", Modifier.padding(top = 0.dp))
                Card(
                    shape = MaterialTheme.shapes.large,
                    colors = CardDefaults.cardColors(containerColor = MaterialTheme.colorScheme.surfaceContainerHigh),
                    modifier = Modifier.fillMaxWidth(),
                ) {
                    Column(Modifier.padding(start = 16.dp, end = 4.dp, top = 4.dp, bottom = 4.dp)) {
                        ui.steps.forEach { step -> StepRow(step) { actions.stepLog(step.key) } }
                    }
                }
                if (ui.phases.isNotEmpty()) {
                    SelectionContainer {
                        Text(
                            ui.phases.joinToString("\n"),
                            style = MaterialTheme.typography.bodySmall,
                            modifier = Modifier.padding(horizontal = 8.dp),
                        )
                    }
                }
                if (ui.facts.isNotEmpty()) {
                    SectionTitle("This step, from its log")
                    Facts(ui.facts)
                }
                if (ui.machine.isNotEmpty()) {
                    SectionTitle("VM and phone")
                    Facts(ui.machine)
                }
            }
        }
    }
}

@Composable
private fun StepRow(step: CreateStep, onLog: () -> Unit) {
    val scheme = MaterialTheme.colorScheme
    Row(verticalAlignment = Alignment.CenterVertically, modifier = Modifier.heightIn(min = 48.dp)) {
        Box(Modifier.size(24.dp), contentAlignment = Alignment.Center) {
            when (step.state) {
                "done" -> Icon(Icons.Filled.Check, contentDescription = "Done", tint = scheme.primary, modifier = Modifier.size(20.dp))
                "running" -> Icon(Icons.Filled.PlayArrow, contentDescription = "Running", tint = scheme.primary, modifier = Modifier.size(20.dp))
                "failed" -> Icon(Icons.Filled.Close, contentDescription = "Failed", tint = scheme.error, modifier = Modifier.size(20.dp))
                else -> Text("·", color = scheme.onSurfaceVariant, modifier = Modifier.semantics { contentDescription = "Not started" })
            }
        }
        Spacer(Modifier.width(12.dp))
        Column(Modifier.weight(1f).alpha(if (step.state == "pending") 0.7f else 1f)) {
            Text(
                step.title,
                style = MaterialTheme.typography.bodyLarge,
                fontWeight = if (step.current || step.state == "failed") FontWeight.Bold else FontWeight.Normal,
            )
            if (step.time.isNotEmpty()) {
                Text(step.time, style = MaterialTheme.typography.bodySmall, color = scheme.onSurfaceVariant)
            }
        }
        TextButton(onClick = onLog, modifier = Modifier.semantics { contentDescription = "Log of ${step.title}" }) { Text("Log") }
    }
}

/** Facts in two columns; a wide one takes a row to itself. */
@Composable
private fun Facts(facts: List<Fact>) {
    val scheme = MaterialTheme.colorScheme
    val rows = ArrayList<List<Fact>>()
    var pair = ArrayList<Fact>()
    for (f in facts) {
        if (f.wide) {
            if (pair.isNotEmpty()) rows += pair
            pair = ArrayList()
            rows += listOf(f)
        } else {
            pair += f
            if (pair.size == 2) {
                rows += pair
                pair = ArrayList()
            }
        }
    }
    if (pair.isNotEmpty()) rows += pair
    SelectionContainer {
        Column(Modifier.padding(horizontal = 8.dp), verticalArrangement = Arrangement.spacedBy(4.dp)) {
            rows.forEach { row ->
                Row(horizontalArrangement = Arrangement.spacedBy(12.dp)) {
                    row.forEach { f ->
                        Text(
                            buildAnnotatedString {
                                withStyle(SpanStyle(color = scheme.onSurfaceVariant)) { append(f.label) }
                                append("  ")
                                if (f.warn) {
                                    withStyle(SpanStyle(color = scheme.error, fontWeight = FontWeight.Bold)) { append(f.value) }
                                } else {
                                    append(f.value)
                                }
                            },
                            fontSize = 12.sp,
                            lineHeight = 16.sp,
                            modifier = Modifier.weight(1f),
                        )
                    }
                    if (row.size == 1 && !row[0].wide) Spacer(Modifier.weight(1f))
                }
            }
        }
    }
}

// ---------------------------------------------------------------------- the VM's vitals

/** What the VM's process is doing during setup: its vitals and each thread's CPU use. */
@OptIn(ExperimentalMaterial3Api::class, ExperimentalMaterial3ExpressiveApi::class)
@Composable
fun CreateVmVitalsScreen(status: String, threads: String, onBack: () -> Unit) {
    Scaffold(
        topBar = {
            MediumFlexibleTopAppBar(
                title = { Text("VM activity") },
                subtitle = { Text("During setup") },
                navigationIcon = {
                    IconButton(onClick = onBack) { Icon(Icons.AutoMirrored.Filled.ArrowBack, contentDescription = "Back to progress") }
                },
            )
        },
    ) { inner ->
        SelectionContainer(Modifier.padding(inner)) {
            Column(Modifier.fillMaxSize().verticalScroll(rememberScrollState()).padding(16.dp), verticalArrangement = Arrangement.spacedBy(12.dp)) {
                Card(
                    shape = MaterialTheme.shapes.extraLarge,
                    colors = CardDefaults.cardColors(containerColor = MaterialTheme.colorScheme.secondaryContainer),
                    modifier = Modifier.fillMaxWidth(),
                ) { Text(status, style = MaterialTheme.typography.bodyLarge, modifier = Modifier.padding(20.dp)) }
                Text(threads, fontFamily = FontFamily.Monospace, fontSize = 12.sp, lineHeight = 16.sp)
                Text(
                    "Setup has no live picture yet. Each thread's history and native stacks are in Live log under VM.",
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
        }
    }
}
