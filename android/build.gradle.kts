plugins {
    id("com.android.application") version "9.5.0" apply false
    // The Compose compiler follows Kotlin: this also sets the Kotlin that AGP's built-in support uses.
    id("org.jetbrains.kotlin.plugin.compose") version "2.4.21" apply false
}

// All build output stays under android/build, which is ignored.
allprojects {
    layout.buildDirectory.set(rootDir.resolve("build/gradle/${project.name}"))
}
