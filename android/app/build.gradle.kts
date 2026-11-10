plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.plugin.compose")
}

android {
    namespace = "com.vphone.android"
    compileSdk = 37

    defaultConfig {
        applicationId = "com.vphone.android"
        minSdk = 35
        targetSdk = 37
        versionCode = 41
        versionName = "0.7.2"
        ndk {
            abiFilters += "arm64-v8a"
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    sourceSets {
        getByName("main") {
            // Staged by scripts/build-apk.sh.
            jniLibs.directories.add(rootDir.resolve("build/jniLibs").path)
        }
    }

    buildFeatures {
        viewBinding = false
        buildConfig = true
        compose = true
    }

    testOptions {
        unitTests {
            // The screenshot tests draw the real resources and theme (Robolectric).
            isIncludeAndroidResources = true
            all {
                // Roborazzi writes PNGs only when recording; they land under android/build, which is ignored.
                it.systemProperty("roborazzi.test.record", "true")
                it.systemProperty("vphone.screenshots", rootDir.resolve("build/ui-screenshots").path)
                it.systemProperty("robolectric.graphicsMode", "NATIVE")
                it.maxHeapSize = "3g"
            }
        }
    }

    packaging {
        jniLibs {
            // Real files on disk: libadrenotools and the driver's namespaces load them by path.
            useLegacyPackaging = true
            keepDebugSymbols += "**/*.so"
        }
    }
}

dependencies {
    // The UI is Jetpack Compose with Material 3 Expressive. The BOM pins Compose itself;
    // material3 is pinned above the BOM's 1.4.0, which has no expressive components.
    implementation(platform("androidx.compose:compose-bom:2026.09.00"))
    implementation("androidx.compose.material3:material3:1.5.0-beta02")
    implementation("androidx.compose.foundation:foundation")
    implementation("androidx.compose.ui:ui")
    implementation("androidx.compose.material:material-icons-core")
    // Material's shapes as paths, for the VM screen's floating button, which is a plain View.
    implementation("androidx.graphics:graphics-shapes:1.0.1")
    implementation("androidx.activity:activity-compose:1.13.0")
    implementation("androidx.core:core-ktx:1.19.1")
    // The creation log's rows: a RecyclerView paged from the Rust log view.
    implementation("androidx.recyclerview:recyclerview:1.4.0")

    // Screenshots of the screens on the JVM, with fake state: scripts/ui-screenshots.sh.
    testImplementation("junit:junit:4.13.2")
    testImplementation("org.robolectric:robolectric:4.17")
    testImplementation("io.github.takahirom.roborazzi:roborazzi:1.77.0")
    testImplementation("io.github.takahirom.roborazzi:roborazzi-compose:1.77.0")
    testImplementation("androidx.compose.ui:ui-test-junit4")
    debugImplementation("androidx.compose.ui:ui-test-manifest")
}
