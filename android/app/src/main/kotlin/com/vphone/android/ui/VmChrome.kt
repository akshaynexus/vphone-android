package com.vphone.android.ui

import androidx.compose.animation.core.Animatable
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.interaction.MutableInteractionSource
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.aspectRatio
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.safeDrawingPadding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.text.selection.SelectionContainer
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Home
import androidx.compose.material.icons.filled.Lock
import androidx.compose.material.icons.filled.Menu
import androidx.compose.material3.Button
import androidx.compose.material3.ExperimentalMaterial3ExpressiveApi
import androidx.compose.material3.FilledTonalButton
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.remember
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp

/** One tile of the VM screen's menu. `danger` marks the one that ends the VM. */
class ControlItem(val icon: ImageVector, val label: String, val danger: Boolean = false, val action: () -> Unit)

/**
 * The VM screen's menu: the guest's picture dimmed, and a panel of tiles that springs in. A tap
 * outside closes it. It is in the window only while it is open, so it is never between a finger
 * and the guest.
 */
@OptIn(ExperimentalMaterial3ExpressiveApi::class)
@Composable
fun VmControlsMenu(heading: String, hint: String, items: List<ControlItem>, onDismiss: () -> Unit, animate: Boolean = true) {
    val shown = remember { Animatable(if (animate) 0f else 1f) }
    val spring = MaterialTheme.motionScheme.fastSpatialSpec<Float>()
    LaunchedEffect(Unit) { shown.animateTo(1f, spring) }
    Box(
        Modifier
            .fillMaxSize()
            .background(Color.Black.copy(alpha = 0.6f * shown.value.coerceIn(0f, 1f)))
            .clickable(interactionSource = remember { MutableInteractionSource() }, indication = null, onClickLabel = "Close the menu", onClick = onDismiss),
        contentAlignment = Alignment.Center,
    ) {
        Surface(
            shape = MaterialTheme.shapes.extraLarge,
            color = MaterialTheme.colorScheme.surfaceContainerHigh,
            tonalElevation = 6.dp,
            modifier = Modifier
                .padding(24.dp)
                .widthIn(max = 380.dp)
                .graphicsLayer {
                    val s = 0.8f + 0.2f * shown.value
                    scaleX = s
                    scaleY = s
                    alpha = shown.value.coerceIn(0f, 1f)
                }
                // A tap between tiles leaves it open.
                .clickable(interactionSource = remember { MutableInteractionSource() }, indication = null, onClick = {}),
        ) {
            Column(Modifier.padding(20.dp), horizontalAlignment = Alignment.CenterHorizontally) {
                Text(heading, style = MaterialTheme.typography.titleMedium, textAlign = TextAlign.Center)
                Column(Modifier.padding(top = 16.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
                    items.chunked(3).forEach { row ->
                        Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                            row.forEach { item -> ControlTile(item, Modifier.weight(1f), onDismiss) }
                            repeat(3 - row.size) { Spacer(Modifier.weight(1f)) }
                        }
                    }
                }
                if (hint.isNotEmpty()) {
                    Text(
                        hint,
                        style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                        textAlign = TextAlign.Center,
                        modifier = Modifier.padding(top = 14.dp),
                    )
                }
            }
        }
    }
}

@Composable
private fun ControlTile(item: ControlItem, modifier: Modifier, onDismiss: () -> Unit) {
    val scheme = MaterialTheme.colorScheme
    Surface(
        onClick = {
            onDismiss()
            item.action()
        },
        shape = MaterialTheme.shapes.large,
        color = if (item.danger) scheme.errorContainer else scheme.secondaryContainer,
        contentColor = if (item.danger) scheme.onErrorContainer else scheme.onSecondaryContainer,
        modifier = modifier.aspectRatio(1f).semantics { contentDescription = item.label },
    ) {
        Column(horizontalAlignment = Alignment.CenterHorizontally, verticalArrangement = Arrangement.Center) {
            Icon(item.icon, contentDescription = null, modifier = Modifier.size(28.dp))
            Text(
                item.label,
                style = MaterialTheme.typography.labelLarge,
                textAlign = TextAlign.Center,
                maxLines = 1,
                modifier = Modifier.padding(top = 8.dp),
            )
        }
    }
}

/** Home, Lock and the menu as a bar under the guest's picture, for those who choose it. */
@Composable
fun VmBottomBar(onHome: () -> Unit, onLock: () -> Unit, onMore: () -> Unit) {
    Row(
        Modifier.fillMaxSize().background(Color.Black).padding(horizontal = 12.dp),
        horizontalArrangement = Arrangement.spacedBy(8.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        val button: @Composable (String, ImageVector, () -> Unit) -> Unit = { label, icon, action ->
            FilledTonalButton(onClick = action, modifier = Modifier.weight(1f)) {
                Icon(icon, contentDescription = null, modifier = Modifier.size(18.dp))
                Spacer(Modifier.width(6.dp))
                Text(label, maxLines = 1)
            }
        }
        button("Home", Icons.Filled.Home, onHome)
        button("Lock", Icons.Filled.Lock, onLock)
        button("More", Icons.Filled.Menu, onMore)
    }
}

/** The VM ended: why, whole and copyable, and where to go. */
@Composable
fun VmEndPanel(title: String, message: String, onLog: () -> Unit, onLeave: () -> Unit, onCopy: () -> Unit) {
    Surface(color = MaterialTheme.colorScheme.surface, modifier = Modifier.fillMaxSize()) {
        Column(
            Modifier.safeDrawingPadding().verticalScroll(rememberScrollState()).padding(24.dp),
            verticalArrangement = Arrangement.spacedBy(12.dp),
        ) {
            Spacer(Modifier.height(24.dp))
            Text(title, style = MaterialTheme.typography.headlineMedium)
            if (message.isNotEmpty()) {
                SelectionContainer { Text(message, style = MaterialTheme.typography.bodyLarge) }
            }
            Button(onClick = onLeave, modifier = Modifier.fillMaxWidth()) { Text("Back to VMs") }
            FilledTonalButton(onClick = onLog, modifier = Modifier.fillMaxWidth()) { Text("Show the log") }
            if (message.isNotEmpty()) TextButton(onClick = onCopy, modifier = Modifier.fillMaxWidth()) { Text("Copy") }
        }
    }
}
