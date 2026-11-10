package com.vphone.android.ui

import androidx.activity.compose.BackHandler
import androidx.compose.foundation.background
import androidx.compose.foundation.interaction.MutableInteractionSource
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.calculateEndPadding
import androidx.compose.foundation.layout.calculateStartPadding
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.grid.GridCells
import androidx.compose.foundation.lazy.grid.GridItemSpan
import androidx.compose.foundation.lazy.grid.LazyGridItemSpanScope
import androidx.compose.foundation.lazy.grid.LazyVerticalGrid
import androidx.compose.foundation.lazy.grid.items
import androidx.compose.foundation.text.selection.SelectionContainer
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.Send
import androidx.compose.material.icons.filled.Add
import androidx.compose.material.icons.filled.Close
import androidx.compose.material.icons.filled.Delete
import androidx.compose.material.icons.filled.PlayArrow
import androidx.compose.material.icons.filled.Settings
import androidx.compose.material3.Badge
import androidx.compose.material3.BadgedBox
import androidx.compose.material3.Button
import androidx.compose.material3.ButtonDefaults
import androidx.compose.material3.ButtonGroup
import androidx.compose.material3.ButtonGroupDefaults
import androidx.compose.material3.ButtonGroupScope
import androidx.compose.material3.Card
import androidx.compose.material3.CardDefaults
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.ExperimentalMaterial3ExpressiveApi
import androidx.compose.material3.FilledTonalButton
import androidx.compose.material3.FloatingActionButtonMenu
import androidx.compose.material3.FloatingActionButtonMenuItem
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.LargeFlexibleTopAppBar
import androidx.compose.material3.LinearWavyProgressIndicator
import androidx.compose.material3.LoadingIndicator
import androidx.compose.material3.MaterialShapes
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Scaffold
import androidx.compose.material3.SplitButtonDefaults
import androidx.compose.material3.SplitButtonLayout
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.ToggleFloatingActionButton
import androidx.compose.material3.ToggleFloatingActionButtonDefaults.animateIcon
import androidx.compose.material3.TopAppBarDefaults
import androidx.compose.material3.toShape
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.input.nestedscroll.nestedScroll
import androidx.compose.ui.platform.LocalLayoutDirection
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.semantics.stateDescription
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp

/** What a VM is doing, as its card says it. */
sealed interface VmStatus {
    data object Stopped : VmStatus

    /** `phase` as [com.vphone.android.VmService.phaseText] words it; `settled` once iOS is up. */
    data class Running(val phase: String, val elapsed: String, val settled: Boolean) : VmStatus

    /** Being made by the wizard: the step it is on, and how far that step is when it says. */
    data class Creating(val text: String, val fraction: Float?, val paused: Boolean, val failed: Boolean = false) : VmStatus

    /** Being packed into a `.vphone` file. */
    data class Sending(val fraction: Float?) : VmStatus
}

data class VmCardUi(
    val name: String,
    val problems: List<String> = emptyList(),
    val width: Int = 0,
    val height: Int = 0,
    val vcpus: Int = 0,
    val ramMib: Int = 0,
    val diskBytes: Long = 0,
    val status: VmStatus = VmStatus.Stopped,
) {
    /** "1179 × 2556", "6 vCPUs", "6 GB RAM", "64 GB disk"; nothing for a VM that is not whole yet. */
    val specs: List<String>
        get() = if (width <= 0) {
            emptyList()
        } else {
            listOf(
                "$width × $height",
                "$vcpus vCPUs",
                if (ramMib % 1024 == 0) "${ramMib / 1024} GB RAM" else "$ramMib MiB RAM",
                "${diskBytes / (1024 * MIB)} GB disk",
            )
        }
}

sealed interface UpdateUi {
    data object Hidden : UpdateUi
    data object Checking : UpdateUi
    data class Available(val version: String, val sizeMb: Long) : UpdateUi
    data class Downloading(val version: String, val done: Long, val total: Long, val bytesPerSec: Long) : UpdateUi
    data class Ready(val version: String) : UpdateUi
    data class Installing(val version: String) : UpdateUi

    /** `canDownload`: the release is known, so the next step is the download and not the check. */
    data class Failed(val message: String, val canDownload: Boolean) : UpdateUi
}

data class FailureUi(val vm: String, val message: String, val hasLog: Boolean)

data class MainUi(
    val version: String,
    val device: String = "",
    val vms: List<VmCardUi> = emptyList(),
    /** Where a VM can be pushed with adb, for the empty list. */
    val vmsPath: String = "",
    val failure: FailureUi? = null,
    val update: UpdateUi = UpdateUi.Hidden,
    val folderImport: ProgressUi? = null,
    val transfer: ProgressUi? = null,
)

class MainActions(
    val boot: (String) -> Unit = {},
    val open: () -> Unit = {},
    val continueCreate: () -> Unit = {},
    val settings: (String) -> Unit = {},
    val send: (String) -> Unit = {},
    val log: (VmCardUi) -> Unit = {},
    val delete: (String) -> Unit = {},
    val create: () -> Unit = {},
    val importFile: () -> Unit = {},
    val importUsb: () -> Unit = {},
    val importFolder: () -> Unit = {},
    val storage: () -> Unit = {},
    val checkUpdates: () -> Unit = {},
    val updatePrimary: () -> Unit = {},
    val updateNotes: () -> Unit = {},
    val updateCancel: () -> Unit = {},
    val cancelImport: () -> Unit = {},
    val cancelTransfer: () -> Unit = {},
    val failureLog: () -> Unit = {},
    val copy: (String) -> Unit = {},
)

@OptIn(ExperimentalMaterial3Api::class, ExperimentalMaterial3ExpressiveApi::class)
@Composable
fun MainScreen(ui: MainUi, actions: MainActions, fabInitiallyOpen: Boolean = false) {
    val scroll = TopAppBarDefaults.exitUntilCollapsedScrollBehavior()
    var fabOpen by rememberSaveable { mutableStateOf(fabInitiallyOpen) }
    BackHandler(fabOpen) { fabOpen = false }
    val updateWaiting = ui.update is UpdateUi.Available || ui.update is UpdateUi.Ready || ui.update is UpdateUi.Failed
    Scaffold(
        modifier = Modifier.nestedScroll(scroll.nestedScrollConnection),
        topBar = {
            LargeFlexibleTopAppBar(
                title = { Text("vphone") },
                subtitle = {
                    Text(
                        if (ui.device.isEmpty()) "Version ${ui.version}" else "Version ${ui.version} · ${ui.device}",
                        maxLines = 2,
                        overflow = TextOverflow.Ellipsis,
                    )
                },
                actions = {
                    IconButton(onClick = actions.checkUpdates) {
                        BadgedBox(badge = { if (updateWaiting) Badge() }) {
                            Icon(
                                VIcons.Update,
                                contentDescription = if (updateWaiting) "Check for updates: one is waiting" else "Check for updates",
                            )
                        }
                    }
                    IconButton(onClick = actions.storage) { Icon(VIcons.Storage, contentDescription = "Storage") }
                },
                scrollBehavior = scroll,
            )
        },
        floatingActionButton = {
            FloatingActionButtonMenu(
                expanded = fabOpen,
                button = {
                    ToggleFloatingActionButton(
                        checked = fabOpen,
                        onCheckedChange = { fabOpen = it },
                        modifier = Modifier.semantics {
                            contentDescription = "Add a VM"
                            stateDescription = if (fabOpen) "Menu open" else "Menu closed"
                        },
                    ) {
                        Icon(
                            if (checkedProgress > 0.5f) Icons.Filled.Close else Icons.Filled.Add,
                            contentDescription = null,
                            modifier = Modifier.animateIcon({ checkedProgress }),
                        )
                    }
                },
            ) {
                val item: @Composable (String, ImageVector, () -> Unit) -> Unit = { label, icon, action ->
                    FloatingActionButtonMenuItem(
                        onClick = {
                            fabOpen = false
                            action()
                        },
                        text = { Text(label) },
                        icon = { Icon(icon, contentDescription = null) },
                    )
                }
                item("Create a VM on this phone", Icons.Filled.Add, actions.create)
                item("Import a .vphone file", VIcons.File, actions.importFile)
                item("Import from a phone by USB", VIcons.Usb, actions.importUsb)
                // One folder import at a time.
                if (ui.folderImport == null) item("Import a VM folder", VIcons.Folder, actions.importFolder)
            }
        },
    ) { inner ->
        val direction = LocalLayoutDirection.current
        LazyVerticalGrid(
            columns = GridCells.Adaptive(minSize = 340.dp),
            modifier = Modifier.fillMaxSize(),
            contentPadding = PaddingValues(
                start = inner.calculateStartPadding(direction) + 16.dp,
                end = inner.calculateEndPadding(direction) + 16.dp,
                top = inner.calculateTopPadding() + 8.dp,
                // Room for the add button under the last card.
                bottom = inner.calculateBottomPadding() + 104.dp,
            ),
            horizontalArrangement = Arrangement.spacedBy(12.dp),
            verticalArrangement = Arrangement.spacedBy(12.dp),
        ) {
            val wide: LazyGridItemSpanScope.() -> GridItemSpan = { GridItemSpan(maxLineSpan) }
            ui.failure?.let { f ->
                item(key = "failure", span = wide) {
                    ErrorCard(
                        title = "${f.vm} stopped",
                        message = f.message,
                        onCopy = { actions.copy("${f.vm}: ${f.message}") },
                        actions = { if (f.hasLog) TextButton(onClick = actions.failureLog) { Text("View the log") } },
                    )
                }
            }
            if (ui.update != UpdateUi.Hidden) {
                item(key = "update", span = wide) { UpdateCard(ui.update, actions) }
            }
            ui.folderImport?.let { p ->
                item(key = "import", span = wide) { ProgressCard(p, VIcons.Folder, onCancel = actions.cancelImport) }
            }
            ui.transfer?.let { p ->
                item(key = "transfer", span = wide) {
                    ProgressCard(p, Icons.AutoMirrored.Filled.Send, onCancel = actions.cancelTransfer)
                }
            }
            if (ui.vms.isEmpty()) {
                item(key = "empty", span = wide) { EmptyVms(ui.vmsPath) }
            }
            items(ui.vms, key = { "vm:${it.name}" }) { vm -> VmCard(vm, actions) }
        }
    }
}

@OptIn(ExperimentalMaterial3ExpressiveApi::class)
@Composable
private fun EmptyVms(path: String) {
    Column(
        Modifier.fillMaxWidth().padding(horizontal = 16.dp, vertical = 40.dp),
        horizontalAlignment = Alignment.CenterHorizontally,
    ) {
        Box(
            Modifier.size(112.dp).background(MaterialTheme.colorScheme.primaryContainer, MaterialShapes.Cookie9Sided.toShape()),
            contentAlignment = Alignment.Center,
        ) {
            Icon(VIcons.Phone, contentDescription = null, tint = MaterialTheme.colorScheme.onPrimaryContainer, modifier = Modifier.size(48.dp))
        }
        Text("No VMs yet", style = MaterialTheme.typography.headlineSmall, modifier = Modifier.padding(top = 20.dp))
        Text(
            "Create one or import one with the + button. A VM can also be pushed with adb to:",
            style = MaterialTheme.typography.bodyMedium,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
            modifier = Modifier.padding(top = 8.dp),
        )
        if (path.isNotEmpty()) {
            SelectionContainer {
                Text(path, style = MaterialTheme.typography.bodySmall, modifier = Modifier.padding(top = 4.dp))
            }
        }
    }
}

/** One VM: its name, what it is doing, its size, and what can be done with it. */
@OptIn(ExperimentalMaterial3ExpressiveApi::class)
@Composable
fun VmCard(vm: VmCardUi, actions: MainActions, modifier: Modifier = Modifier) {
    val status = vm.status
    val running = status is VmStatus.Running
    val scheme = MaterialTheme.colorScheme
    val container = if (running) scheme.primaryContainer else scheme.surfaceContainerHigh
    val badge = if (running) scheme.primary else scheme.secondaryContainer
    val onBadge = if (running) scheme.onPrimary else scheme.onSecondaryContainer
    Card(
        modifier = modifier.fillMaxWidth(),
        shape = MaterialTheme.shapes.extraLarge,
        colors = CardDefaults.cardColors(containerColor = container),
    ) {
        Column(Modifier.padding(start = 20.dp, end = 20.dp, top = 20.dp, bottom = 12.dp)) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                Box(
                    Modifier.size(52.dp).background(
                        badge,
                        (if (running) MaterialShapes.Cookie9Sided else MaterialShapes.Square).toShape(),
                    ),
                    contentAlignment = Alignment.Center,
                ) {
                    when {
                        status is VmStatus.Running && !status.settled -> LoadingIndicator(color = onBadge, modifier = Modifier.size(36.dp))
                        status is VmStatus.Creating && !status.paused && !status.failed ->
                            LoadingIndicator(color = onBadge, modifier = Modifier.size(36.dp))
                        else -> Icon(VIcons.Phone, contentDescription = null, tint = onBadge)
                    }
                }
                Spacer(Modifier.width(16.dp))
                Column(Modifier.weight(1f)) {
                    Text(vm.name, style = MaterialTheme.typography.titleLarge, maxLines = 2, overflow = TextOverflow.Ellipsis)
                    Text(
                        statusText(status, vm.problems.isNotEmpty()),
                        style = MaterialTheme.typography.labelLarge,
                        color = when {
                            vm.problems.isNotEmpty() || (status is VmStatus.Creating && status.failed) -> scheme.error
                            running -> scheme.onPrimaryContainer
                            else -> scheme.onSurfaceVariant
                        },
                    )
                }
            }
            val fraction = when (status) {
                is VmStatus.Creating -> if (status.paused || status.failed) -1f else status.fraction
                is VmStatus.Sending -> status.fraction
                else -> -1f
            }
            if (fraction == null) {
                LinearWavyProgressIndicator(Modifier.fillMaxWidth().padding(top = 14.dp).semantics { contentDescription = "Working" })
            } else if (fraction >= 0f) {
                LinearWavyProgressIndicator(
                    progress = { fraction.coerceIn(0f, 1f) },
                    modifier = Modifier.fillMaxWidth().padding(top = 14.dp).semantics { contentDescription = "Progress" },
                )
            }
            if (vm.problems.isNotEmpty()) {
                SelectionContainer {
                    Text(
                        vm.problems.joinToString("\n"),
                        style = MaterialTheme.typography.bodyMedium,
                        color = scheme.error,
                        modifier = Modifier.padding(top = 12.dp),
                    )
                }
            } else if (vm.specs.isNotEmpty()) {
                StatRow(
                    vm.specs,
                    Modifier.padding(top = 14.dp),
                    color = if (running) scheme.surfaceContainerLowest.copy(alpha = 0.6f) else scheme.surfaceContainerLowest,
                )
            }
            val ok = vm.problems.isEmpty()
            ButtonGroup(
                overflowIndicator = { menu -> ButtonGroupDefaults.OverflowIndicator(menu) },
                // Not fillMaxWidth: with a minimum width, material3 1.5.0-beta02's ButtonGroup throws
                // while measuring its overflow button once the actions do not all fit.
                modifier = Modifier.padding(top = 12.dp),
                horizontalArrangement = Arrangement.spacedBy(8.dp),
            ) {
                when (status) {
                    is VmStatus.Running -> action("Open", Icons.Filled.PlayArrow, primary = true) { actions.open() }
                    is VmStatus.Creating -> action("Continue", Icons.Filled.PlayArrow, primary = true) { actions.continueCreate() }
                    else -> action("Boot", Icons.Filled.PlayArrow, primary = true, enabled = ok && status is VmStatus.Stopped) { actions.boot(vm.name) }
                }
                if (status !is VmStatus.Creating) {
                    action("Settings", Icons.Filled.Settings, onColour = running) { actions.settings(vm.name) }
                    action("Send", Icons.AutoMirrored.Filled.Send, enabled = ok, onColour = running) { actions.send(vm.name) }
                }
                action("Log", VIcons.Log, onColour = running) { actions.log(vm) }
                action("Delete…", Icons.Filled.Delete, onColour = running) { actions.delete(vm.name) }
            }
        }
    }
}

private fun statusText(status: VmStatus, broken: Boolean): String = when (status) {
    is VmStatus.Running -> if (status.elapsed.isEmpty()) status.phase else "${status.phase} · ${status.elapsed}"
    is VmStatus.Creating -> when {
        status.failed -> "Creation stopped: ${status.text}"
        status.paused -> "Creation paused: ${status.text}"
        else -> "Being created: ${status.text}"
    }
    is VmStatus.Sending -> "Being packed to send"
    VmStatus.Stopped -> if (broken) "Cannot start" else "Stopped"
}

/** A button of a card's group; in the group's overflow menu it is a menu item. */
@OptIn(ExperimentalMaterial3ExpressiveApi::class)
private fun ButtonGroupScope.action(
    label: String,
    icon: ImageVector,
    primary: Boolean = false,
    enabled: Boolean = true,
    /** On the running VM's coloured card, where the usual tonal button would not stand out. */
    onColour: Boolean = false,
    onClick: () -> Unit,
) {
    customItem(
        buttonGroupContent = {
            val source = remember { MutableInteractionSource() }
            val content: @Composable () -> Unit = {
                Icon(icon, contentDescription = null, modifier = Modifier.size(18.dp))
                Spacer(Modifier.width(6.dp))
                Text(label, maxLines = 1)
            }
            val padding = PaddingValues(horizontal = 14.dp, vertical = 8.dp)
            if (primary) {
                Button(onClick, Modifier.animateWidth(source), enabled, contentPadding = padding, interactionSource = source) { content() }
            } else {
                val scheme = MaterialTheme.colorScheme
                FilledTonalButton(
                    onClick,
                    Modifier.animateWidth(source),
                    enabled,
                    colors = if (onColour) {
                        ButtonDefaults.filledTonalButtonColors(containerColor = scheme.surfaceContainerLowest.copy(alpha = 0.6f), contentColor = scheme.onSurface)
                    } else {
                        ButtonDefaults.filledTonalButtonColors()
                    },
                    contentPadding = padding,
                    interactionSource = source,
                ) { content() }
            }
        },
        menuContent = { menu ->
            DropdownMenuItem(
                text = { Text(label) },
                leadingIcon = { Icon(icon, contentDescription = null) },
                enabled = enabled,
                onClick = {
                    menu.dismiss()
                    onClick()
                },
            )
        },
    )
}

/** What the updater found and is doing, with the next step as the main button. */
@OptIn(ExperimentalMaterial3ExpressiveApi::class)
@Composable
fun UpdateCard(ui: UpdateUi, actions: MainActions, modifier: Modifier = Modifier) {
    when (ui) {
        UpdateUi.Hidden -> Unit
        UpdateUi.Checking -> BusyCard("Checking for updates", modifier)
        is UpdateUi.Installing -> BusyCard("Installing ${ui.version}: the app restarts when it is done", modifier)
        is UpdateUi.Available -> OfferCard(
            "vphone ${ui.version} is available",
            "A ${ui.sizeMb} MB download. On Wi-Fi it starts by itself.",
            "Download and install",
            actions,
            modifier,
        )
        is UpdateUi.Ready -> OfferCard(
            "vphone ${ui.version} is downloaded",
            "Installing replaces the app and restarts it. Your VMs stay.",
            "Install",
            actions,
            modifier,
        )
        is UpdateUi.Downloading -> {
            val left = if (ui.bytesPerSec > 0 && ui.total > ui.done) leftText((ui.total - ui.done) / ui.bytesPerSec) else ""
            ProgressCard(
                ProgressUi(
                    title = "Downloading vphone ${ui.version}",
                    fraction = if (ui.total > 0) ui.done.toFloat() / ui.total else null,
                    stats = listOf("${ui.done / MIB} of ${ui.total / MIB} MB", rateText(ui.bytesPerSec), left),
                    cancelLabel = "Cancel",
                ),
                VIcons.Download,
                modifier,
                onCancel = actions.updateCancel,
                containerColor = MaterialTheme.colorScheme.tertiaryContainer,
            )
        }
        is UpdateUi.Failed -> ErrorCard(
            title = "The update did not work",
            message = ui.message,
            modifier = modifier,
            onCopy = { actions.copy(ui.message) },
            actions = { TextButton(onClick = actions.updatePrimary) { Text(if (ui.canDownload) "Download" else "Try again") } },
        )
    }
}

@OptIn(ExperimentalMaterial3ExpressiveApi::class)
@Composable
private fun BusyCard(text: String, modifier: Modifier) {
    Card(
        modifier = modifier.fillMaxWidth(),
        shape = MaterialTheme.shapes.extraLarge,
        colors = CardDefaults.cardColors(containerColor = MaterialTheme.colorScheme.tertiaryContainer),
    ) {
        Row(Modifier.padding(20.dp), verticalAlignment = Alignment.CenterVertically) {
            LoadingIndicator(Modifier.size(40.dp).semantics { contentDescription = "Working" })
            Spacer(Modifier.width(16.dp))
            Text(text, style = MaterialTheme.typography.titleMedium)
        }
    }
}

@OptIn(ExperimentalMaterial3ExpressiveApi::class)
@Composable
private fun OfferCard(title: String, detail: String, primary: String, actions: MainActions, modifier: Modifier) {
    var menu by remember { mutableStateOf(false) }
    Card(
        modifier = modifier.fillMaxWidth(),
        shape = MaterialTheme.shapes.extraLarge,
        colors = CardDefaults.cardColors(containerColor = MaterialTheme.colorScheme.tertiaryContainer),
    ) {
        Column(Modifier.padding(20.dp)) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                Box(
                    Modifier.size(48.dp).background(MaterialTheme.colorScheme.tertiary, MaterialShapes.SoftBurst.toShape()),
                    contentAlignment = Alignment.Center,
                ) { Icon(VIcons.Download, contentDescription = null, tint = MaterialTheme.colorScheme.onTertiary) }
                Spacer(Modifier.width(16.dp))
                Column {
                    Text(title, style = MaterialTheme.typography.titleMedium)
                    Text(detail, style = MaterialTheme.typography.bodyMedium)
                }
            }
            Row(Modifier.fillMaxWidth().padding(top = 16.dp), horizontalArrangement = Arrangement.End) {
                SplitButtonLayout(
                    leadingButton = {
                        SplitButtonDefaults.LeadingButton(onClick = actions.updatePrimary) { Text(primary) }
                    },
                    trailingButton = {
                        Box {
                            SplitButtonDefaults.TrailingButton(
                                checked = menu,
                                onCheckedChange = { menu = it },
                                modifier = Modifier.semantics {
                                    contentDescription = "More about this update"
                                    stateDescription = if (menu) "Open" else "Closed"
                                },
                            ) { Icon(VIcons.Expand, contentDescription = null, modifier = Modifier.size(SplitButtonDefaults.TrailingIconSize)) }
                            DropdownMenu(expanded = menu, onDismissRequest = { menu = false }) {
                                DropdownMenuItem(
                                    text = { Text("What's new") },
                                    onClick = {
                                        menu = false
                                        actions.updateNotes()
                                    },
                                )
                            }
                        }
                    },
                )
            }
        }
    }
}
