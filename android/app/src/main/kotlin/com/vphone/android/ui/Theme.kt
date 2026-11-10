package com.vphone.android.ui

import androidx.compose.foundation.isSystemInDarkTheme
import androidx.compose.material3.ColorScheme
import androidx.compose.material3.ExperimentalMaterial3ExpressiveApi
import androidx.compose.material3.MaterialExpressiveTheme
import androidx.compose.material3.MotionScheme
import androidx.compose.material3.darkColorScheme
import androidx.compose.material3.dynamicDarkColorScheme
import androidx.compose.material3.dynamicLightColorScheme
import androidx.compose.material3.lightColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalContext

/** The scheme for a phone with no wallpaper colours to give: blues, with a warm third colour. */
val FallbackLight: ColorScheme = lightColorScheme(
    primary = Color(0xFF345CA8),
    onPrimary = Color(0xFFFFFFFF),
    primaryContainer = Color(0xFFD8E2FF),
    onPrimaryContainer = Color(0xFF14448F),
    secondary = Color(0xFF565E71),
    onSecondary = Color(0xFFFFFFFF),
    secondaryContainer = Color(0xFFDAE2F9),
    onSecondaryContainer = Color(0xFF3E4759),
    tertiary = Color(0xFF8A5021),
    onTertiary = Color(0xFFFFFFFF),
    tertiaryContainer = Color(0xFFFFDCC5),
    onTertiaryContainer = Color(0xFF6D390B),
    error = Color(0xFFBA1A1A),
    onError = Color(0xFFFFFFFF),
    errorContainer = Color(0xFFFFDAD6),
    onErrorContainer = Color(0xFF93000A),
    background = Color(0xFFF9F9FF),
    onBackground = Color(0xFF1A1B20),
    surface = Color(0xFFF9F9FF),
    onSurface = Color(0xFF1A1B20),
    surfaceVariant = Color(0xFFE1E2EC),
    onSurfaceVariant = Color(0xFF44474F),
    outline = Color(0xFF757780),
    outlineVariant = Color(0xFFC5C6D0),
    inverseSurface = Color(0xFF2F3036),
    inverseOnSurface = Color(0xFFF0F0F7),
    inversePrimary = Color(0xFFAEC6FF),
    surfaceDim = Color(0xFFD9D9E0),
    surfaceBright = Color(0xFFF9F9FF),
    surfaceContainerLowest = Color(0xFFFFFFFF),
    surfaceContainerLow = Color(0xFFF3F3FA),
    surfaceContainer = Color(0xFFEDEDF4),
    surfaceContainerHigh = Color(0xFFE8E7EF),
    surfaceContainerHighest = Color(0xFFE2E2E9),
)

val FallbackDark: ColorScheme = darkColorScheme(
    primary = Color(0xFFAEC6FF),
    onPrimary = Color(0xFF002E69),
    primaryContainer = Color(0xFF14448F),
    onPrimaryContainer = Color(0xFFD8E2FF),
    secondary = Color(0xFFBEC6DC),
    onSecondary = Color(0xFF283041),
    secondaryContainer = Color(0xFF3E4759),
    onSecondaryContainer = Color(0xFFDAE2F9),
    tertiary = Color(0xFFFFB783),
    onTertiary = Color(0xFF502400),
    tertiaryContainer = Color(0xFF6D390B),
    onTertiaryContainer = Color(0xFFFFDCC5),
    error = Color(0xFFFFB4AB),
    onError = Color(0xFF690005),
    errorContainer = Color(0xFF93000A),
    onErrorContainer = Color(0xFFFFDAD6),
    background = Color(0xFF111318),
    onBackground = Color(0xFFE2E2E9),
    surface = Color(0xFF111318),
    onSurface = Color(0xFFE2E2E9),
    surfaceVariant = Color(0xFF44474F),
    onSurfaceVariant = Color(0xFFC5C6D0),
    outline = Color(0xFF8E9099),
    outlineVariant = Color(0xFF44474F),
    inverseSurface = Color(0xFFE2E2E9),
    inverseOnSurface = Color(0xFF2F3036),
    inversePrimary = Color(0xFF345CA8),
    surfaceDim = Color(0xFF111318),
    surfaceBright = Color(0xFF37393E),
    surfaceContainerLowest = Color(0xFF0C0E13),
    surfaceContainerLow = Color(0xFF1A1B20),
    surfaceContainer = Color(0xFF1E1F25),
    surfaceContainerHigh = Color(0xFF282A2F),
    surfaceContainerHighest = Color(0xFF33353A),
)

/**
 * Material 3 Expressive for every screen: the phone's own colours (Material You) where it has
 * them, the fallback scheme otherwise, and the expressive motion, shapes and type.
 */
@OptIn(ExperimentalMaterial3ExpressiveApi::class)
@Composable
fun VphoneTheme(
    darkTheme: Boolean = isSystemInDarkTheme(),
    dynamicColor: Boolean = true,
    content: @Composable () -> Unit,
) {
    val context = LocalContext.current
    val colors = when {
        dynamicColor -> if (darkTheme) dynamicDarkColorScheme(context) else dynamicLightColorScheme(context)
        darkTheme -> FallbackDark
        else -> FallbackLight
    }
    MaterialExpressiveTheme(colorScheme = colors, motionScheme = MotionScheme.expressive(), content = content)
}
